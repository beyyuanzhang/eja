use crate::config::ConnectorConfig;
use crate::http::truncate_utf8;
use crate::mcp::{ConnectorPool, ConnectorStatus};
use crate::sandbox::{kill_process_tree, Sandbox, TrustedDir};
use crate::state::{schedule_wakes, WakeAlarm};
use eja_core::{Call, Eja, Outcome, Timestamp};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SHELL_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub capability: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub is_kernel: bool,
}

fn builtin_spec(name: &str, description: &str, schema: Value, is_kernel: bool) -> ToolSpec {
    ToolSpec {
        capability: name.into(),
        name: name.into(),
        description: description.into(),
        schema,
        is_kernel,
    }
}

pub fn activity_label(tool_name: &str) -> &str {
    match tool_name {
        "shell" => "执行 Shell",
        "search" | "read" => "查阅记忆",
        "keep" | "release" => "更新挂念",
        "wake" => "预约唤醒",
        other => other,
    }
}

pub struct Hands {
    data_dir: TrustedDir,
    connectors: ConnectorPool,
}

impl Hands {
    pub fn open(data_dir: TrustedDir, sandbox: Sandbox, connectors: &[ConnectorConfig]) -> Self {
        let mut pool = ConnectorPool::new(sandbox);
        pool.reload(connectors, false);
        Self {
            data_dir,
            connectors: pool,
        }
    }

    pub fn reload(&mut self, connectors: &[ConnectorConfig], force: bool) {
        self.connectors.reload(connectors, force);
    }

    pub fn connector_statuses(&self) -> Vec<ConnectorStatus> {
        self.connectors.statuses()
    }

    pub fn tools(&self) -> Vec<ToolSpec> {
        let mut specs = vec![
            builtin_spec(
                "keep",
                "新建或修订一条跨窗口挂念。",
                json!({
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer", "description": "修订已有挂念时传入其 id；新建时省略" },
                        "text": { "type": "string" }
                    },
                    "required": ["text"]
                }),
                true,
            ),
            builtin_spec(
                "release",
                "放下不再需要关注的开放挂念。",
                json!({
                    "type": "object",
                    "properties": { "concern": { "type": "integer" } },
                    "required": ["concern"]
                }),
                true,
            ),
            builtin_spec(
                "read",
                "按事实 id 读取完整历史记录及其最新回执/修订。",
                json!({
                    "type": "object",
                    "properties": {
                        "fact": { "type": "integer" },
                        "offset": { "type": "integer", "minimum": 0, "description": "分页续读时的字节偏移，默认 0" }
                    },
                    "required": ["fact"]
                }),
                true,
            ),
            builtin_spec(
                "search",
                "从全历史主体日志中检索相关旧事实。",
                json!({
                    "type": "object",
                    "properties": { "cue": { "type": "string" } },
                    "required": ["cue"]
                }),
                true,
            ),
            builtin_spec(
                "wake",
                "预约未来主动唤醒。",
                json!({
                    "type": "object",
                    "properties": {
                        "delay_seconds": { "type": "integer", "minimum": 1 },
                        "reason": { "type": "string" }
                    },
                    "required": ["delay_seconds", "reason"]
                }),
                false,
            ),
            builtin_spec(
                "shell",
                "执行本机 shell 命令。",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "cwd": { "type": "string" },
                        "timeout_seconds": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["command"]
                }),
                false,
            ),
        ];

        let mut used: HashSet<String> = specs.iter().map(|spec| spec.name.clone()).collect();

        for tool in self.connectors.tools() {
            let name = safe_tool_name(&tool.capability, &mut used);
            let description = if tool.description.is_empty() {
                format!("MCP 工具 {}", tool.capability)
            } else {
                tool.description
            };
            specs.push(ToolSpec {
                capability: tool.capability,
                name,
                description,
                schema: tool.input_schema,
                is_kernel: false,
            });
        }
        specs
    }

    pub fn execute(&mut self, call: &Call, eja: &Eja, sandbox: &Sandbox) -> Outcome {
        match call.capability.as_str() {
            "wake" => execute_wake(eja, call.id, &call.input),
            "shell" => self.execute_shell(sandbox, call.id, &call.input),
            other if other.starts_with("mcp.") => self.connectors.call(other, &call.input),
            other => Outcome::Failed(format!("未知能力: {other}")),
        }
    }

    fn execute_shell(&self, sandbox: &Sandbox, call_id: eja_core::Id, input: &Value) -> Outcome {
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
            .unwrap_or_else(|| self.data_dir.path().to_path_buf());
        let timeout = input
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .filter(|&secs| secs >= 1)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_SHELL_TIMEOUT);
        run_shell(sandbox, command, &cwd, timeout, &self.data_dir, call_id)
    }
}

fn execute_wake(eja: &Eja, call_id: eja_core::Id, input: &Value) -> Outcome {
    let delay = input
        .get("delay_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reason = input
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if delay < 1 || reason.is_empty() {
        return Outcome::Failed("wake 需要 delay_seconds >= 1 且非空 reason".into());
    }
    let now = eja.now().unwrap_or_else(|_| Timestamp::now()).as_unix();
    let Some(fire_at) = now.checked_add(delay) else {
        return Outcome::Failed("唤醒时间超出范围".into());
    };
    let display = format_local_unix(fire_at);
    let alarm = WakeAlarm {
        id: format!("w-{fire_at}-{call_id}"),
        fire_at_unix: fire_at,
        fire_at_display: display.clone(),
        reason: reason.to_string(),
    };
    match schedule_wakes(eja, &[alarm]) {
        Ok(()) => Outcome::Observed(format!("scheduled at {display} ({reason})")),
        Err(err) => Outcome::Failed(format!("保存唤醒失败: {err}")),
    }
}

fn capture_shell_stream(mut reader: impl Read) -> io::Result<(tempfile::SpooledTempFile, u64)> {
    let mut spool = tempfile::spooled_tempfile(MAX_SHELL_BYTES);
    let len = io::copy(&mut reader, &mut spool)?;
    spool.rewind()?;
    Ok((spool, len))
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
        Ok(path) => Ok(format!("{preview}\n[output truncated at {MAX_SHELL_BYTES} bytes; full {total} bytes saved to {}]", path.display())),
        Err(err) => Ok(format!("{preview}\n[完整输出保存失败: {err}]")),
    }
}

fn run_shell(
    sandbox: &Sandbox,
    command: &str,
    cwd: &Path,
    timeout: Duration,
    data_dir: &TrustedDir,
    call_id: eja_core::Id,
) -> Outcome {
    let mut child = match sandbox
        .command("/bin/sh", ["-c", command])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return Outcome::Failed(format!("启动 shell 失败: {err}")),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_handle = thread::spawn(move || capture_shell_stream(stdout));
    let err_handle = thread::spawn(move || capture_shell_stream(stderr));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) if out_handle.is_finished() && err_handle.is_finished() => break Some(st),
            Ok(_) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            _ => {
                kill_process_tree(&mut child);
                break None;
            }
        }
    };
    let bounded = match (out_handle.join(), err_handle.join()) {
        (Ok(Ok(out)), Ok(Ok(err))) => shell_output(out, err, data_dir, call_id),
        (Ok(Err(err)), _) | (_, Ok(Err(err))) => Err(err),
        _ => Err(io::Error::other("输出读取线程中断")),
    };
    let bounded = match bounded {
        Ok(text) => text,
        Err(err) => return Outcome::Unknown(format!("读取 Shell 输出失败: {err}")),
    };
    match status {
        Some(st) if st.success() => Outcome::Observed(bounded),
        Some(st) => Outcome::Failed(format!("exit {}: {bounded}", st.code().unwrap_or(-1))),
        None => Outcome::Unknown(format!("命令超时 ({}s): {bounded}", timeout.as_secs())),
    }
}

fn safe_tool_name(capability: &str, used: &mut HashSet<String>) -> String {
    let mut base: String = capability
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .take(56)
        .collect();
    if base.is_empty() {
        base = "mcp_tool".to_string();
    }
    let mut candidate = base.clone();
    let mut idx = 2usize;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}_{idx}");
        idx += 1;
    }
    candidate
}

fn format_local_unix(secs: i64) -> String {
    Command::new("/bin/date")
        .args(["-r", &secs.to_string(), "+%H:%M:%S"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
