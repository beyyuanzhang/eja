use crate::config::ConnectorConfig;
use crate::mcp::{truncate_utf8, ConnectorPool, ConnectorStatus};
use crate::sandbox::{kill_process_tree, write_new_file_in_subdir, Sandbox};
use eja_core::{Call, Eja, Outcome, WakeAlarm};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KERNEL_TOOL_NAMES: &[&str] = &["keep", "release", "read", "search"];
const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SHELL_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub capability: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub struct Hands {
    data_dir: PathBuf,
    connectors: ConnectorPool,
}

impl Hands {
    pub fn open(data_dir: PathBuf, sandbox: Sandbox, connectors: &[ConnectorConfig]) -> Self {
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
            ToolSpec {
                capability: "wake".into(),
                name: "wake".into(),
                description: "预约未来主动唤醒。".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "delay_seconds": { "type": "integer", "minimum": 1 },
                        "reason": { "type": "string" }
                    },
                    "required": ["delay_seconds", "reason"]
                }),
            },
            ToolSpec {
                capability: "shell".into(),
                name: "shell".into(),
                description: "执行本机 shell 命令。".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "cwd": { "type": "string" },
                        "timeout_seconds": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["command"]
                }),
            },
        ];

        let mut used: HashSet<String> = KERNEL_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .chain(specs.iter().map(|spec| spec.name.clone()))
            .collect();

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
            .unwrap_or_else(|| self.data_dir.clone());
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
    let Some(fire_at) = unix_now().checked_add(delay) else {
        return Outcome::Failed("唤醒时间超出范围".into());
    };
    let display = format_local_unix(fire_at);
    let alarm = WakeAlarm {
        id: format!("w-{fire_at}-{call_id}"),
        fire_at_unix: fire_at,
        fire_at_display: display.clone(),
        reason: reason.to_string(),
    };
    match eja.schedule_wakes(&[alarm]) {
        Ok(()) => Outcome::Observed(format!("scheduled at {display} ({reason})")),
        Err(err) => Outcome::Failed(format!("保存唤醒失败: {err}")),
    }
}

fn run_shell(
    sandbox: &Sandbox,
    command: &str,
    cwd: &Path,
    timeout: Duration,
    data_dir: &Path,
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
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stdout {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });
    let err_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });

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
    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();
    let combined = match (out.trim(), err.trim()) {
        ("", "") => "(no output)".to_string(),
        (o, "") => o.to_string(),
        ("", e) => e.to_string(),
        (o, e) => format!("{o}\n[stderr]\n{e}"),
    };
    let bounded = if combined.len() > MAX_SHELL_BYTES {
        let preview = truncate_utf8(&combined, MAX_SHELL_BYTES);
        let file_name = format!("shell-{call_id}.log");
        if let Ok(spill_path) =
            write_new_file_in_subdir(data_dir, "outputs", &file_name, combined.as_bytes())
        {
            format!(
                "{preview}\n[output truncated at {} bytes; full {} bytes saved to {}]",
                MAX_SHELL_BYTES,
                combined.len(),
                spill_path.display()
            )
        } else {
            preview
        }
    } else {
        combined
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

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        let sandbox = Sandbox::new(&core_dir).unwrap();

        // stdin is /dev/null so a stdin reader exits immediately instead of hanging.
        let stdin_check = run_shell(
            &sandbox,
            "cat",
            dir.path(),
            Duration::from_secs(2),
            dir.path(),
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
            dir.path(),
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
            dir.path(),
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

    #[cfg(unix)]
    #[test]
    fn shell_spill_refuses_symlinked_outputs_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        let sandbox = Sandbox::new(&core_dir).unwrap();
        symlink(&core_dir, data_dir.join("outputs")).unwrap();

        let outcome = run_shell(
            &sandbox,
            "python3 -c 'print(\"x\" * 40000)'",
            &data_dir,
            Duration::from_secs(5),
            &data_dir,
            7,
        );
        assert!(matches!(outcome, Outcome::Observed(ref text) if !text.contains("saved to")));
        assert!(!core_dir.join("shell-7.log").exists());
    }
}
