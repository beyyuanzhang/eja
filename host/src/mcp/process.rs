use crate::http::truncate_utf8;
use crate::sandbox::{kill_process_tree, write_process_input, ProcessOutput, Sandbox, TrustedDir};
use crate::tool::{ModelImage, ToolExecution, MAX_TOOL_IMAGE_BYTES};
use eja_core::Outcome;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SHELL_BYTES: usize = 32 * 1024;

pub(super) enum ProcessInput<'a> {
    Shell,
    Exec(&'a [u8]),
}

pub(super) fn execute_shell(
    sandbox: &Sandbox,
    data_dir: &TrustedDir,
    call_id: eja_core::Id,
    input: &Value,
) -> Outcome {
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if command.is_empty() {
        return Outcome::Failed("shell 需要非空 command".into());
    }
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.path().to_path_buf());
    let timeout = input
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .filter(|&secs| secs >= 1)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SHELL_TIMEOUT);
    run_process(
        sandbox,
        "/bin/sh",
        &["-c".into(), command.into()],
        &BTreeMap::new(),
        ProcessInput::Shell,
        &cwd,
        timeout,
        data_dir,
        call_id,
    )
    .outcome
}

fn capture_shell_stream(mut reader: impl Read) -> io::Result<(tempfile::SpooledTempFile, u64)> {
    let mut spool = tempfile::spooled_tempfile(MAX_SHELL_BYTES);
    let len = io::copy(&mut reader, &mut spool)?;
    spool.rewind()?;
    Ok((spool, len))
}

enum ProcessStdout {
    Text((tempfile::SpooledTempFile, u64)),
    Image(ModelImage),
    OversizedImage,
}

fn capture_stdout(mut reader: impl Read, is_exec: bool) -> io::Result<ProcessStdout> {
    if !is_exec {
        return capture_shell_stream(reader).map(ProcessStdout::Text);
    }
    let mut prefix = Vec::with_capacity(12);
    reader.by_ref().take(12).read_to_end(&mut prefix)?;
    let Some(media_type) = ModelImage::media_type(&prefix) else {
        return capture_shell_stream(prefix.as_slice().chain(reader)).map(ProcessStdout::Text);
    };
    reader
        .by_ref()
        .take((MAX_TOOL_IMAGE_BYTES + 1 - prefix.len()) as u64)
        .read_to_end(&mut prefix)?;
    if prefix.len() > MAX_TOOL_IMAGE_BYTES {
        io::copy(&mut reader, &mut io::sink())?;
        return Ok(ProcessStdout::OversizedImage);
    }
    Ok(ProcessStdout::Image(ModelImage {
        media_type,
        bytes: prefix,
    }))
}

fn shell_output(
    (mut stdout, out_len): (tempfile::SpooledTempFile, u64),
    (mut stderr, err_len): (tempfile::SpooledTempFile, u64),
    data_dir: &TrustedDir,
    call_id: eja_core::Id,
) -> io::Result<String> {
    let preview = |stream: &mut tempfile::SpooledTempFile| -> io::Result<String> {
        let mut bytes = Vec::new();
        stream
            .take(MAX_SHELL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    };
    let out = preview(&mut stdout)?;
    let err = preview(&mut stderr)?;
    let text = match (out.is_empty(), err.is_empty()) {
        (true, true) => "(no output)".to_owned(),
        (false, true) => out,
        (true, false) => err,
        (false, false) => format!("{out}\n[stderr]\n{err}"),
    };
    let separator = if out_len > 0 && err_len > 0 {
        "\n[stderr]\n"
    } else {
        ""
    };
    let total = out_len + err_len + separator.len() as u64;
    if total <= MAX_SHELL_BYTES as u64 && text.len() <= MAX_SHELL_BYTES {
        return Ok(text);
    }
    let preview = truncate_utf8(&text, MAX_SHELL_BYTES);
    stdout.rewind()?;
    stderr.rewind()?;
    let stream = stdout.chain(separator.as_bytes()).chain(stderr);
    match data_dir.write_new_file_in_subdir("outputs", &format!("shell-{call_id}.log"), stream) {
        Ok(path) => Ok(format!(
            "{preview}\n[output truncated at {MAX_SHELL_BYTES} bytes; full {total} bytes saved to {}]",
            path.display()
        )),
        Err(err) => Ok(format!("{preview}\n[完整输出保存失败: {err}]")),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_process(
    sandbox: &Sandbox,
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    input: ProcessInput<'_>,
    cwd: &Path,
    timeout: Duration,
    data_dir: &TrustedDir,
    call_id: eja_core::Id,
) -> ToolExecution {
    let resolved = if program.contains('/') && Path::new(program).is_relative() {
        cwd.join(program).to_string_lossy().into_owned()
    } else {
        program.to_string()
    };
    let started = Instant::now();
    let deadline = started
        .checked_add(timeout)
        .unwrap_or_else(|| started + DEFAULT_SHELL_TIMEOUT);
    let mut child = match sandbox
        .command(&resolved, args)
        .current_dir(cwd)
        .envs(env)
        .env("EJA_DATA_DIR", data_dir.path())
        .stdin(if matches!(input, ProcessInput::Exec(_)) {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return Outcome::Failed(format!("启动工具进程失败: {err}")).into(),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let is_exec = matches!(input, ProcessInput::Exec(_));
    let out_handle =
        thread::spawn(move || capture_stdout(ProcessOutput::new(stdout, deadline)?, is_exec));
    let err_handle =
        thread::spawn(move || capture_shell_stream(ProcessOutput::new(stderr, deadline)?));

    let input_error = match input {
        ProcessInput::Exec(bytes) => {
            let mut stdin = child.stdin.take().expect("piped stdin");
            write_process_input(&mut stdin, bytes, deadline).err()
        }
        ProcessInput::Shell => None,
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) if out_handle.is_finished() && err_handle.is_finished() => break Some(st),
            Ok(_)
                if Instant::now() < deadline && !sandbox.cancel_flag().load(Ordering::Relaxed) =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            _ => {
                kill_process_tree(&mut child);
                break None;
            }
        }
    };
    let stopped = if sandbox.cancel_flag().load(Ordering::Relaxed) {
        "已中止".to_string()
    } else {
        format!("命令超时 ({}s)", timeout.as_secs())
    };
    let succeeded = status.is_some_and(|status| status.success()) && input_error.is_none();
    let bounded = match (out_handle.join(), err_handle.join()) {
        (Ok(Ok(stdout)), Ok(Ok(stderr))) => {
            let text = match stdout {
                ProcessStdout::Text(text) => text,
                ProcessStdout::Image(image) if succeeded => {
                    return ToolExecution {
                        outcome: Outcome::Observed(image.receipt()),
                        images: vec![image],
                    };
                }
                ProcessStdout::OversizedImage if succeeded => {
                    return Outcome::Observed(format!(
                        "工具已完成，图像超过 {MAX_TOOL_IMAGE_BYTES} 字节"
                    ))
                    .into();
                }
                _ => (tempfile::spooled_tempfile(MAX_SHELL_BYTES), 0),
            };
            shell_output(text, stderr, data_dir, call_id)
        }
        (Ok(Err(err)), _) | (_, Ok(Err(err))) => Err(err),
        _ => Err(io::Error::other("输出读取线程中断")),
    };
    let bounded = match bounded {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::TimedOut => {
            return Outcome::Unknown(format!("命令超时 ({}s)", timeout.as_secs())).into()
        }
        Err(err) => return Outcome::Unknown(format!("读取进程输出失败: {err}")).into(),
    };
    if let Some(err) = input_error {
        if err.kind() == io::ErrorKind::TimedOut || status.is_none() {
            return Outcome::Unknown(format!("{stopped}: {bounded}")).into();
        }
        return Outcome::Unknown(format!("工具输入未完整写入: {err}\n{bounded}")).into();
    }
    match status {
        Some(st) if st.success() => Outcome::Observed(bounded),
        Some(st) => Outcome::Failed(format!("exit {}: {bounded}", st.code().unwrap_or(-1))),
        None => Outcome::Unknown(format!("{stopped}: {bounded}")),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn run_shell(
        sandbox: &Sandbox,
        command: &str,
        cwd: &Path,
        timeout: Duration,
        data_dir: &TrustedDir,
        call_id: eja_core::Id,
    ) -> Outcome {
        run_process(
            sandbox,
            "/bin/sh",
            &["-c".into(), command.into()],
            &BTreeMap::new(),
            ProcessInput::Shell,
            cwd,
            timeout,
            data_dir,
            call_id,
        )
        .outcome
    }

    #[test]
    fn shell_closes_stdin_and_reaps_pipeline_grandchildren_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = TrustedDir::open(dir.path()).unwrap();
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        let sandbox = Sandbox::new(&core_dir).unwrap();

        // stdin is /dev/null so a stdin reader exits immediately instead of hanging.
        let stdin_check = run_shell(
            &sandbox,
            "cat",
            dir.path(),
            Duration::from_secs(2),
            &data_root,
            1,
        );
        assert_eq!(stdin_check, Outcome::Observed("(no output)".into()));

        // Pipeline grandchildren holding stdout open must be killed on timeout without hanging join.
        let pid_file = dir.path().join("grandchild.pid");
        let cmd = format!("(sleep 30 & echo $! > {}; wait) | cat", pid_file.display());
        let started = Instant::now();
        let outcome = run_shell(
            &sandbox,
            &cmd,
            dir.path(),
            Duration::from_secs(1),
            &data_root,
            2,
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(matches!(outcome, Outcome::Unknown(ref msg) if msg.contains("命令超时")));

        let started = Instant::now();
        let outcome = run_shell(
            &sandbox,
            "sleep 5 & printf ready",
            dir.path(),
            Duration::from_secs(1),
            &data_root,
            3,
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(matches!(outcome, Outcome::Unknown(ref msg) if msg.contains("命令超时")));

        if let Ok(pid_text) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_text.trim().parse::<i32>() {
                let alive = Command::new("/bin/kill")
                    .args(["-0", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                assert!(!alive, "grandchild process {pid} should be terminated");
            }
        }
    }

    #[test]
    fn shell_output_spools_large_streams_and_preserves_full_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = TrustedDir::open(dir.path()).unwrap();
        let stdout = "中".repeat(MAX_SHELL_BYTES);
        let stderr = b"end\n";
        let out = capture_shell_stream(stdout.as_bytes()).unwrap();
        assert!(out.0.is_rolled());
        let err = capture_shell_stream(stderr.as_slice()).unwrap();
        assert!(!err.0.is_rolled());
        let preview = shell_output(out, err, &root, 12).unwrap();
        assert!(preview.starts_with(&stdout[..90]));
        assert!(preview.contains("saved to"));
        assert!(preview.len() < MAX_SHELL_BYTES + 512);
        let log = fs::read(dir.path().join("outputs/shell-12.log")).unwrap();
        assert_eq!(log, [stdout.as_bytes(), b"\n[stderr]\n", stderr].concat());
    }

    #[cfg(unix)]
    #[test]
    fn shell_spill_refuses_symlinked_outputs_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        let data_root = TrustedDir::open(&data_dir).unwrap();
        let sandbox = Sandbox::new(&core_dir).unwrap();
        symlink(&core_dir, data_dir.join("outputs")).unwrap();

        let outcome = run_shell(
            &sandbox,
            "python3 -c 'print(\"x\" * 40000)'",
            &data_dir,
            Duration::from_secs(5),
            &data_root,
            7,
        );
        assert!(matches!(outcome, Outcome::Observed(ref text) if !text.contains("saved to")));
        assert!(!core_dir.join("shell-7.log").exists());
    }

    #[test]
    fn exec_deadline_includes_input_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let data = TrustedDir::open(dir.path()).unwrap();
        let core = dir.path().join("private");
        fs::create_dir(&core).unwrap();
        let sandbox = Sandbox::new(&core).unwrap();
        let input = vec![b' '; 256 * 1024];
        let started = Instant::now();
        let result = run_process(
            &sandbox,
            "/bin/sleep",
            &["5".into()],
            &BTreeMap::new(),
            ProcessInput::Exec(&input),
            dir.path(),
            Duration::from_millis(150),
            &data,
            1,
        );
        assert!(matches!(result.outcome, Outcome::Unknown(ref msg) if msg.contains("命令超时")));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancel_flag_stops_a_running_process_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let data = TrustedDir::open(dir.path()).unwrap();
        let core = dir.path().join("private");
        fs::create_dir(&core).unwrap();
        let sandbox = Sandbox::new(&core).unwrap();
        let flag = std::sync::Arc::clone(sandbox.cancel_flag());
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let outcome = run_shell(
            &sandbox,
            "sleep 30",
            dir.path(),
            Duration::from_secs(60),
            &data,
            1,
        );
        stopper.join().unwrap();
        assert!(matches!(outcome, Outcome::Unknown(ref msg) if msg.starts_with("已中止")));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
