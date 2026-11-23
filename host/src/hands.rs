use crate::config::ConnectorConfig;
use crate::http::truncate_utf8;
use crate::mcp::{ConnectorPool, ConnectorStatus};
use crate::sandbox::{kill_process_tree, Sandbox, TrustedDir};
use crate::state::{schedule_wakes, GuardProbe, GuardTrigger, WakeAlarm, WakeGuard};
use crate::tools::{
    active_verified_tools, ensure_verified_tool_ready, execute_verified_tool,
    normalize_tool_capability, promote_tool, run_verified_tool_probe, ScriptRunOutcome,
};
use eja_core::{Call, Eja, Outcome, Timestamp};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SHELL_BYTES: usize = 32 * 1024;
const GUARD_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_GUARD_PROBE_BYTES: usize = 4 * 1024;

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
        "promote_tool" => "校验并注册工具",
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

    pub fn tools(&self, eja: &Eja) -> Vec<ToolSpec> {
        let mut specs = vec![
            builtin_spec(
                "keep",
                "新建或修订一条跨窗口挂念；可附带 evidence_fact_id 与客观验收条件 verify_criteria。",
                json!({
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer", "description": "修订已有挂念时传入其 id；新建时省略" },
                        "text": { "type": "string" },
                        "evidence_fact_id": { "type": "integer", "description": "可选：支撑该挂念的来源事实 id" },
                        "verify_criteria": { "type": "string", "description": "可选：客观验收标准（设置后默认 release 时要求提供 witness_fact_id）" },
                        "require_witness": { "type": "boolean", "description": "可选：是否强制 release 时校验 witness_fact_id" }
                    },
                    "required": ["text"]
                }),
                true,
            ),
            builtin_spec(
                "release",
                "放下不再需要关注的开放挂念；对带验收约束的挂念必须提供指向 observed/person/world 的 witness_fact_id。",
                json!({
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer" },
                        "witness_fact_id": { "type": "integer", "description": "验证该挂念已完成/可放下的客观事实 id" }
                    },
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
                "预约未来主动唤醒；可选附带只读 guard_command 或已封签 guard_tool 探针，仅当条件命中时才写入世界事件并唤醒模型。",
                json!({
                    "type": "object",
                    "properties": {
                        "delay_seconds": { "type": "integer", "minimum": 1 },
                        "reason": { "type": "string" },
                        "guard_command": { "type": "string", "description": "可选：沙箱内只读探针命令，未命中时不写日志也不唤醒模型（与 guard_tool 互斥）" },
                        "guard_tool": { "type": "string", "description": "可选：已通过 promote_tool 封签的用户空间工具名（如 watch_metric 或 tool.watch_metric），与 guard_command 互斥" },
                        "guard_input": { "type": "object", "description": "可选：传给 guard_tool 的 JSON 输入参数，默认 {}" },
                        "interval_seconds": { "type": "integer", "minimum": 1, "description": "未命中时的复查间隔秒数，默认等于 delay_seconds" },
                        "trigger_on": { "type": "string", "enum": ["changed", "exit_zero", "non_empty"], "description": "命中条件：changed(默认)/exit_zero/non_empty" },
                        "max_checks": { "type": "integer", "minimum": 1, "description": "可选：最多检查次数" },
                        "failure_threshold": { "type": "integer", "minimum": 1, "description": "探针连续异常达到此次数后上报模型，默认 3" }
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
            builtin_spec(
                "promote_tool",
                "校验并注册用户空间 tools/<name>/ 下的确定性工具（需通过 check 脚本才会封签启用）。",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "tools/<name>/ 目录名" }
                    },
                    "required": ["name"]
                }),
                false,
            ),
        ];

        let mut used: HashSet<String> = specs.iter().map(|spec| spec.name.clone()).collect();

        for tool in active_verified_tools(&self.data_dir, eja) {
            let name = safe_tool_name(&tool.capability, &mut used);
            specs.push(ToolSpec {
                capability: tool.capability,
                name,
                description: tool.description,
                schema: tool.schema,
                is_kernel: false,
            });
        }

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
            "wake" => execute_wake(&self.data_dir, eja, sandbox, call.id, &call.input),
            "shell" => self.execute_shell(sandbox, call.id, &call.input),
            "promote_tool" => {
                let name = call
                    .input
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                promote_tool(&self.data_dir, eja, sandbox, name)
            }
            other if other.starts_with("tool.") => {
                execute_verified_tool(&self.data_dir, eja, sandbox, other, &call.input)
            }
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

pub enum GuardProbeOutcome {
    Ok { output: String, digest: String },
    NonZero { code: i32, output: String },
    Unknown(String),
}

pub fn run_guard_probe(
    data_dir: &TrustedDir,
    eja: &Eja,
    sandbox: &Sandbox,
    probe: &GuardProbe,
) -> GuardProbeOutcome {
    match probe {
        GuardProbe::Command { command } => run_command_probe(sandbox, command, data_dir.path()),
        GuardProbe::Tool { capability, input } => match run_verified_tool_probe(
            data_dir,
            eja,
            sandbox,
            capability,
            input,
            GUARD_PROBE_TIMEOUT,
            MAX_GUARD_PROBE_BYTES,
        ) {
            ScriptRunOutcome::Ok(output) => {
                let digest = format!("{:x}", Sha256::digest(output.as_bytes()));
                GuardProbeOutcome::Ok { output, digest }
            }
            ScriptRunOutcome::NonZero { code, output } => {
                GuardProbeOutcome::NonZero { code, output }
            }
            ScriptRunOutcome::Unknown(msg) => GuardProbeOutcome::Unknown(msg),
        },
    }
}

fn run_command_probe(sandbox: &Sandbox, command: &str, cwd: &Path) -> GuardProbeOutcome {
    let mut child = match sandbox
        .command("/bin/sh", ["-c", command])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return GuardProbeOutcome::Unknown(format!("启动探针失败: {err}")),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_handle = thread::spawn(move || read_bounded_probe(stdout));
    let err_handle = thread::spawn(move || read_bounded_probe(stderr));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) if out_handle.is_finished() && err_handle.is_finished() => break Some(st),
            Ok(_) if started.elapsed() < GUARD_PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(20))
            }
            _ => {
                kill_process_tree(&mut child);
                break None;
            }
        }
    };
    let out = out_handle.join().ok().and_then(Result::ok).unwrap_or_default();
    let err = err_handle.join().ok().and_then(Result::ok).unwrap_or_default();
    let combined = match (out.trim().is_empty(), err.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => out.trim().to_string(),
        (true, false) => err.trim().to_string(),
        (false, false) => format!("{}\n[stderr]\n{}", out.trim(), err.trim()),
    };
    match status {
        Some(st) if st.success() => {
            let digest = format!("{:x}", Sha256::digest(combined.as_bytes()));
            GuardProbeOutcome::Ok {
                output: truncate_utf8(&combined, MAX_GUARD_PROBE_BYTES),
                digest,
            }
        }
        Some(st) => GuardProbeOutcome::NonZero {
            code: st.code().unwrap_or(-1),
            output: truncate_utf8(&combined, MAX_GUARD_PROBE_BYTES),
        },
        None => GuardProbeOutcome::Unknown(format!(
            "探针超时 ({}s): {}",
            GUARD_PROBE_TIMEOUT.as_secs(),
            truncate_utf8(&combined, MAX_GUARD_PROBE_BYTES)
        )),
    }
}

fn read_bounded_probe(mut reader: impl Read) -> io::Result<String> {
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(MAX_GUARD_PROBE_BYTES as u64 + 1)
        .read_to_end(&mut buf)?;
    let _ = io::copy(&mut reader, &mut io::sink());
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub fn format_wake_display(secs: i64) -> String {
    format_local_unix(secs)
}

fn execute_wake(
    data_dir: &TrustedDir,
    eja: &Eja,
    sandbox: &Sandbox,
    call_id: eja_core::Id,
    input: &Value,
) -> Outcome {
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
    let guard_cmd = input
        .get("guard_command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let guard_tool = input
        .get("guard_tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if guard_cmd.is_some() && guard_tool.is_some() {
        return Outcome::Failed("guard_command 与 guard_tool 不能同时指定".into());
    }

    let probe = if let Some(cmd) = guard_cmd {
        Some(GuardProbe::Command {
            command: cmd.to_string(),
        })
    } else if let Some(tool_raw) = guard_tool {
        let capability = match normalize_tool_capability(tool_raw) {
            Ok(cap) => cap,
            Err(msg) => return Outcome::Failed(msg),
        };
        if let Err(msg) = ensure_verified_tool_ready(data_dir, eja, &capability) {
            return Outcome::Failed(format!("guard_tool 不可用: {msg}"));
        }
        let tool_input = match input.get("guard_input") {
            None | Some(Value::Null) => json!({}),
            Some(v) if v.is_object() => v.clone(),
            Some(_) => return Outcome::Failed("guard_input 必须是 JSON 对象".into()),
        };
        Some(GuardProbe::Tool {
            capability,
            input: tool_input,
        })
    } else {
        None
    };

    let guard = if let Some(probe) = probe {
        let interval = input
            .get("interval_seconds")
            .and_then(Value::as_i64)
            .unwrap_or(delay);
        if interval < 1 {
            return Outcome::Failed("wake guard 需要 interval_seconds >= 1".into());
        }
        let trigger_on = match input.get("trigger_on").and_then(Value::as_str) {
            None | Some("changed") => GuardTrigger::Changed,
            Some("exit_zero") => GuardTrigger::ExitZero,
            Some("non_empty") => GuardTrigger::NonEmpty,
            Some(other) => {
                return Outcome::Failed(format!(
                    "未知 trigger_on: {other}（仅支持 changed/exit_zero/non_empty）"
                ))
            }
        };
        let max_checks = input
            .get("max_checks")
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as u32);
        let failure_threshold = input
            .get("failure_threshold")
            .and_then(Value::as_u64)
            .map(|v| v.max(1) as u32)
            .unwrap_or(3);
        let last_digest = if trigger_on == GuardTrigger::Changed {
            match run_guard_probe(data_dir, eja, sandbox, &probe) {
                GuardProbeOutcome::Ok { digest, .. } => Some(digest),
                GuardProbeOutcome::NonZero { code, output } => {
                    return Outcome::Failed(format!(
                        "guard 基线探针失败 (exit {code}): {output}"
                    ))
                }
                GuardProbeOutcome::Unknown(msg) => {
                    return Outcome::Failed(format!("guard 基线探针异常: {msg}"))
                }
            }
        } else {
            None
        };
        Some(WakeGuard {
            probe,
            interval_seconds: interval,
            trigger_on,
            last_digest,
            max_checks,
            checks_done: 0,
            consecutive_failures: 0,
            failure_threshold,
        })
    } else {
        None
    };

    let now = eja.now().unwrap_or_else(|_| Timestamp::now()).as_unix();
    let Some(fire_at) = now.checked_add(delay) else {
        return Outcome::Failed("唤醒时间超出范围".into());
    };
    let display = format_local_unix(fire_at);
    let guarded_summary = guard.as_ref().map(|g| match &g.probe {
        GuardProbe::Command { .. } => format!(", guarded {:?} every {}s", g.trigger_on, g.interval_seconds),
        GuardProbe::Tool { capability, .. } => format!(
            ", guarded tool {capability} {:?} every {}s",
            g.trigger_on, g.interval_seconds
        ),
    });
    let alarm = WakeAlarm {
        id: format!("w-{fire_at}-{call_id}"),
        fire_at_unix: fire_at,
        fire_at_display: display.clone(),
        reason: reason.to_string(),
        guard,
    };
    match schedule_wakes(eja, &[alarm]) {
        Ok(()) => Outcome::Observed(format!(
            "scheduled at {display} ({reason}{})",
            guarded_summary.unwrap_or_default()
        )),
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
