use crate::config::ConnectorConfig;
use crate::mcp::{truncate_utf8, ConnectorPool, ConnectorStatus};
use eja_core::{Call, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeAlarm {
    pub id: String,
    pub fire_at_unix: i64,
    pub fire_at_display: String,
    pub reason: String,
}

pub struct Hands {
    data_dir: PathBuf,
    connectors: ConnectorPool,
    wakes: Vec<WakeAlarm>,
}

impl Hands {
    pub fn open(data_dir: PathBuf, connectors: &[ConnectorConfig]) -> Self {
        let mut pool = ConnectorPool::default();
        pool.reload(connectors);
        let wakes = load_wakes(&data_dir);
        Self {
            data_dir,
            connectors: pool,
            wakes,
        }
    }

    pub fn reload(&mut self, connectors: &[ConnectorConfig]) {
        self.connectors.reload(connectors);
    }

    pub fn connector_statuses(&self) -> Vec<ConnectorStatus> {
        self.connectors.statuses()
    }

    pub fn wakes(&self) -> Vec<WakeAlarm> {
        self.wakes.clone()
    }

    pub fn drain_due_wakes(&mut self) -> Vec<WakeAlarm> {
        let now = unix_now();
        let mut fired = Vec::new();
        self.wakes.retain(|alarm| {
            if alarm.fire_at_unix <= now {
                fired.push(alarm.clone());
                false
            } else {
                true
            }
        });
        if !fired.is_empty() {
            save_wakes(&self.data_dir, &self.wakes);
        }
        fired
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

    pub fn execute(&mut self, call: &Call) -> Outcome {
        match call.capability.as_str() {
            "wake" => self.execute_wake(call.id, &call.input),
            "shell" => self.execute_shell(call.id, &call.input),
            other if other.starts_with("mcp.") => self.connectors.call(other, &call.input),
            other => Outcome::Failed(format!("未知能力: {other}")),
        }
    }

    fn execute_wake(&mut self, call_id: eja_core::Id, input: &Value) -> Outcome {
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
        let fire_at = unix_now() + delay;
        let display = format_local_unix(fire_at);
        let alarm = WakeAlarm {
            id: format!("w-{fire_at}-{call_id}"),
            fire_at_unix: fire_at,
            fire_at_display: display.clone(),
            reason: reason.to_string(),
        };
        self.wakes.push(alarm);
        self.wakes.sort_by_key(|a| a.fire_at_unix);
        save_wakes(&self.data_dir, &self.wakes);
        Outcome::Observed(format!("scheduled at {display} ({reason})"))
    }

    fn execute_shell(&self, call_id: eja_core::Id, input: &Value) -> Outcome {
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
        run_shell(command, &cwd, timeout, &self.data_dir, call_id)
    }
}

fn run_shell(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    data_dir: &Path,
    call_id: eja_core::Id,
) -> Outcome {
    let mut child = match Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return Outcome::Failed(format!("启动 shell 失败: {err}")),
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut r) = stdout {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });
    let err_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut r) = stderr {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(_) => break None,
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
        let outputs_dir = data_dir.join("outputs");
        let spill_path = outputs_dir.join(format!("shell-{call_id}.log"));
        if fs::create_dir_all(&outputs_dir).is_ok() && fs::write(&spill_path, &combined).is_ok() {
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
        None => Outcome::Failed(format!("命令超时 ({}s): {bounded}", timeout.as_secs())),
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

fn load_wakes(data_dir: &Path) -> Vec<WakeAlarm> {
    fs::read(data_dir.join("wakes.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_wakes(data_dir: &Path, wakes: &[WakeAlarm]) {
    if let Ok(bytes) = serde_json::to_vec_pretty(wakes) {
        let _ = fs::write(data_dir.join("wakes.json"), bytes);
    }
}

fn unix_now() -> i64 {
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
