//! Bounded host subprocess output and cancellation, for checkpoint Git and
//! workspace commands.
use crate::{Control, Result};
use serde_json::{json, Value};
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc,
    },
    time::Duration,
};

/// How often the watcher looks at the run's control while the child runs.
const WATCH: Duration = Duration::from_millis(5);

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the child was spawned into its own process group above, so its
        // id is a live group id owned by this process; killing that group ends the
        // command and anything it started. Killing an already-reaped group is a
        // no-op error that this path ignores on purpose.
        unsafe {
            if let Ok(group) = libc::pid_t::try_from(self.0.id()) {
                libc::killpg(group, libc::SIGKILL);
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A finished run's full account: exit code, bounded streams, and truncation.
struct Finished {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    ended: Option<String>,
}

fn run(command: &mut Command, control: &Control, limit: usize) -> Result<Finished> {
    control.check()?;
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = Owned(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let stdout = child.0.stdout.take().ok_or("missing command stdout")?;
    let stderr = child.0.stderr.take().ok_or("missing command stderr")?;
    // Readers keep draining after the cap, so a noisy child cannot deadlock on a
    // full pipe. Only bounded prefixes are retained.
    let read = move |mut stream: Box<dyn Read + Send>| {
        let mut bytes = Vec::new();
        let mut overflow = false;
        let mut chunk = [0u8; 4096];
        loop {
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let keep = count.min(limit.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk[..keep]);
            overflow |= keep < count;
        }
        Ok::<_, std::io::Error>((bytes, overflow))
    };
    let out = std::thread::spawn(move || read(Box::new(stdout)));
    let err = std::thread::spawn(move || read(Box::new(stderr)));
    // The caller blocks on the child; one watcher enforces its control. Nothing on
    // this path polls the child to discover that it finished.
    let Ok(group) = libc::pid_t::try_from(child.0.id()) else {
        return Err("command has no signalable process group".into());
    };
    let (stop, stopped) = mpsc::channel::<()>();
    let killed = Arc::new(AtomicBool::new(false));
    let watcher = {
        let control = control.clone();
        let killed = killed.clone();
        std::thread::spawn(move || loop {
            match stopped.recv_timeout(WATCH) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if control.check().is_err() {
                killed.store(true, Ordering::SeqCst);
                // SAFETY: same group id as above; the child is alive until reaped.
                unsafe {
                    libc::killpg(group, libc::SIGKILL);
                }
                return;
            }
        })
    };
    let status = child.0.wait();
    drop(stop);
    let _ = watcher.join();
    let (out, err) = (out.join(), err.join());
    let status = status?;
    let (out, out_overflow) = out.map_err(|_| "command stdout reader panicked")??;
    let (err, err_overflow) = err.map_err(|_| "command stderr reader panicked")??;
    // Only a kill the watcher actually delivered ended the run: a command that
    // exits on its own near the deadline is still its own answer.
    let ended = if killed.load(Ordering::SeqCst) {
        control.check().err().map(|error| error.to_string())
    } else {
        None
    };
    Ok(Finished {
        code: status.code(),
        stdout: out,
        stderr: err,
        truncated: out_overflow || err_overflow,
        ended,
    })
}

pub fn output(command: &mut Command, control: &Control, limit: usize) -> Result<Vec<u8>> {
    let finished = run(command, control, limit)?;
    if finished.ended.is_none() && finished.code != Some(0) {
        // A control that expired while the child ran is the reason it died.
        control.check()?;
        return Err(format!(
            "checkpoint command failed: {}",
            String::from_utf8_lossy(&finished.stderr).trim()
        )
        .into());
    }
    if finished.truncated {
        return Err("command output limit reached".into());
    }
    Ok(finished.stdout)
}

/// One workspace command's account: exit code, bounded output, and whether the
/// run's own control ended it. A non-zero exit is an answer, not an error — the
/// caller reads the code; only a spawn failure or an expired control is one.
pub fn shell_output(command: &mut Command, control: &Control, limit: usize) -> Result<Value> {
    let finished = run(command, control, limit)?;
    Ok(json!({
        "exit_code": finished.code,
        "stdout": String::from_utf8_lossy(&finished.stdout),
        "stderr": String::from_utf8_lossy(&finished.stderr),
        "truncated": finished.truncated,
        "ended": finished.ended,
    }))
}
