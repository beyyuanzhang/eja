//! Bounded host subprocess output and cancellation, used for checkpoint Git.
use crate::{Control, Result};
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.0.id() as i32, libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub(crate) fn output(command: &mut Command, control: &Control, limit: usize) -> Result<Vec<u8>> {
    control.check()?;
    #[cfg(unix)]
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
    let status = (|| -> Result<_> {
        loop {
            if let Some(status) = child.0.try_wait()? {
                return Ok(status);
            }
            control.check()?;
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    drop(child);
    let out = out.join();
    let err = err.join();
    let (out, overflow) = out.map_err(|_| "command stdout reader panicked")??;
    let (err, err_overflow) = err.map_err(|_| "command stderr reader panicked")??;
    let status = status?;
    if overflow || err_overflow {
        return Err("command output limit reached".into());
    }
    if !status.success() {
        return Err(format!(
            "checkpoint command failed: {}",
            String::from_utf8_lossy(&err).trim()
        )
        .into());
    }
    Ok(out)
}
