use crate::config::{HostConfig, ProviderConfig};
use crate::llm::{Brain, HttpBrain};
use crate::mcp::{truncate_utf8, ConnectorPool, ConnectorStatus};
use eja_core::{Call, Eja, Entry, Fact, Id, Outcome, Source, Step, PAGE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub type EventSink = Arc<dyn Fn(Value) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeAlarm {
    pub id: String,
    pub fire_at_unix: i64,
    pub fire_at_display: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamItem {
    pub id: Id,
    pub kind: String, // "person" | "eja" | "event"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proactive: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcernDto {
    pub id: Id,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostState {
    pub status: String,
    pub status_detail: String,
    pub data_dir: String,
    pub has_encoder: bool,
    pub configured: bool,
    pub config: HostConfig,
    pub connector_statuses: Vec<ConnectorStatus>,
    pub stream: Vec<StreamItem>,
    pub concerns: Vec<ConcernDto>,
    pub consolidated: Option<String>,
    pub wakes: Vec<WakeAlarm>,
}

pub struct HostRuntime {
    data_dir: PathBuf,
    eja: Mutex<Eja>,
    config: Mutex<HostConfig>,
    config_mtime: Mutex<Option<SystemTime>>,
    connectors: Mutex<ConnectorPool>,
    wakes: Mutex<Vec<WakeAlarm>>,
    status: Mutex<(String, String)>,
    busy: Mutex<()>,
    brain: Arc<dyn Brain>,
    emit: EventSink,
}

impl HostRuntime {
    pub fn open(data_dir: PathBuf, emit: EventSink) -> Result<Self, String> {
        Self::open_with_brain(data_dir, Arc::new(HttpBrain), emit)
    }

    pub fn open_with_brain(
        data_dir: PathBuf,
        brain: Arc<dyn Brain>,
        emit: EventSink,
    ) -> Result<Self, String> {
        fs::create_dir_all(&data_dir).map_err(|err| err.to_string())?;
        let eja = open_subject(&data_dir.join("subject.db"))?;
        let config = HostConfig::load(&data_dir);
        let mtime = file_mtime(&HostConfig::path(&data_dir));
        let mut pool = ConnectorPool::default();
        pool.reload(&config.connectors);
        let wakes = load_wakes(&data_dir);
        Ok(Self {
            data_dir,
            eja: Mutex::new(eja),
            config: Mutex::new(config),
            config_mtime: Mutex::new(mtime),
            connectors: Mutex::new(pool),
            wakes: Mutex::new(wakes),
            status: Mutex::new(("idle".into(), "在线".into())),
            busy: Mutex::new(()),
            brain,
            emit,
        })
    }

    pub fn snapshot(&self) -> HostState {
        let config = self.config.lock().unwrap().clone();
        let (status, status_detail) = self.status.lock().unwrap().clone();
        let connector_statuses = self.connectors.lock().unwrap().statuses();
        let wakes = self.wakes.lock().unwrap().clone();
        let eja = self.eja.lock().unwrap();
        let has_encoder = eja.has_encoder();
        let view = eja.view().ok();
        let entries = load_recent_entries(&eja, 80);
        let concerns = view
            .as_ref()
            .map(|v| {
                v.concerns
                    .iter()
                    .map(|c| ConcernDto {
                        id: c.id,
                        text: c.text.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let consolidated = view.and_then(|v| {
            v.consolidated.and_then(|entry| match entry.fact {
                Fact::Consolidated { text, .. } => Some(text),
                _ => None,
            })
        });
        HostState {
            status,
            status_detail,
            data_dir: self.data_dir.display().to_string(),
            has_encoder,
            configured: config.is_configured(),
            config,
            connector_statuses,
            stream: build_stream(&entries),
            concerns,
            consolidated,
            wakes,
        }
    }

    pub fn emit_state(&self) {
        if let Ok(mut payload) = serde_json::to_value(self.snapshot()) {
            payload["event"] = Value::String("state".into());
            (self.emit)(payload);
        }
    }

    pub fn emit_error(&self, message: impl Into<String>) {
        (self.emit)(json!({ "event": "error", "message": message.into() }));
    }

    pub fn emit_notify(&self, title: &str, body: &str) {
        (self.emit)(json!({ "event": "notify", "title": title, "body": body }));
    }

    pub fn save_config(&self, updated: HostConfig) {
        if let Err(err) = updated.save(&self.data_dir) {
            self.emit_error(format!("保存配置失败: {err}"));
            return;
        }
        let connectors_changed = {
            let mut guard = self.config.lock().unwrap();
            let changed = guard.connectors != updated.connectors;
            *guard = updated.clone();
            changed
        };
        *self.config_mtime.lock().unwrap() = file_mtime(&HostConfig::path(&self.data_dir));
        if connectors_changed {
            self.connectors.lock().unwrap().reload(&updated.connectors);
        }
        self.emit_state();
    }

    pub fn reload_connectors(&self) {
        let config = HostConfig::load(&self.data_dir);
        *self.config.lock().unwrap() = config.clone();
        *self.config_mtime.lock().unwrap() = file_mtime(&HostConfig::path(&self.data_dir));
        self.connectors.lock().unwrap().reload(&config.connectors);
        self.emit_state();
    }

    pub fn test_provider(&self, id: String, provider: ProviderConfig, model: String) {
        let result = self.brain.test_connection(&provider, &model);
        match result {
            Ok(message) => (self.emit)(json!({
                "event": "provider_test",
                "id": id,
                "ok": true,
                "message": message
            })),
            Err(err) => (self.emit)(json!({
                "event": "provider_test",
                "id": id,
                "ok": false,
                "message": err
            })),
        }
    }

    pub fn handle_user_message(&self, raw: &str) {
        let text = raw.trim();
        if text.is_empty() {
            return;
        }
        let _guard = self.busy.lock().unwrap();
        if let Err(err) = self.eja.lock().unwrap().notice(Source::Person, text) {
            self.emit_error(format!("写入消息失败: {err}"));
            return;
        }
        self.emit_state();
        self.run_turn_locked(false);
    }

    pub fn check_due_wakes(&self) {
        let now = unix_now();
        let due: Vec<WakeAlarm> = {
            let mut wakes = self.wakes.lock().unwrap();
            let mut fired = Vec::new();
            wakes.retain(|alarm| {
                if alarm.fire_at_unix <= now {
                    fired.push(alarm.clone());
                    false
                } else {
                    true
                }
            });
            if !fired.is_empty() {
                save_wakes(&self.data_dir, &wakes);
            }
            fired
        };
        if due.is_empty() {
            return;
        }
        let _guard = self.busy.lock().unwrap();
        for alarm in due {
            let notice = format!("定时唤醒：{}", alarm.reason);
            let _ = self.eja.lock().unwrap().notice(Source::Clock, notice);
        }
        self.emit_state();
        self.run_turn_locked(true);
    }

    pub fn consolidate_now(&self, force: bool) {
        let _guard = self.busy.lock().unwrap();
        self.consolidate_locked(force);
    }

    fn sync_config_if_changed(&self) {
        let latest_mtime = file_mtime(&HostConfig::path(&self.data_dir));
        let changed = {
            let mut guard = self.config_mtime.lock().unwrap();
            if *guard != latest_mtime {
                *guard = latest_mtime;
                true
            } else {
                false
            }
        };
        if changed {
            let loaded = HostConfig::load(&self.data_dir);
            let connectors_changed = {
                let mut cfg = self.config.lock().unwrap();
                let diff = cfg.connectors != loaded.connectors;
                *cfg = loaded.clone();
                diff
            };
            if connectors_changed {
                self.connectors.lock().unwrap().reload(&loaded.connectors);
            }
        }
    }

    fn run_turn_locked(&self, proactive: bool) {
        self.sync_config_if_changed();
        let config = self.config.lock().unwrap().clone();
        let Some((provider, model)) = config.resolve_judge() else {
            self.emit_error("请先在设置中配置可用的 LLM Provider 与主模型");
            return;
        };
        let provider = provider.clone();
        let model = model.to_string();
        let mcp_tools = self.connectors.lock().unwrap().tools();
        let mut grant_names: Vec<String> = vec!["say".into(), "wake".into(), "shell".into()];
        grant_names.extend(mcp_tools.iter().map(|t| t.capability.clone()));
        let grant_refs: Vec<&str> = grant_names.iter().map(|s| s.as_str()).collect();

        self.set_status("thinking", "思考中…");
        let terminal_done = Cell::new(false);
        let pending_terminal = Cell::new(false);
        let data_dir_str = self.data_dir.display().to_string();

        let turn_result = {
            let eja = self.eja.lock().unwrap();
            let has_encoder = eja.has_encoder();
            eja.turn(
                &grant_refs,
                10,
                |view| {
                    if terminal_done.get() {
                        return Ok(Step::default());
                    }
                    let now_local = current_local_time();
                    let step = self
                        .brain
                        .judge(
                            &provider,
                            &model,
                            view,
                            &mcp_tools,
                            has_encoder,
                            &now_local,
                            &data_dir_str,
                        )
                        .map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;
                    let only_terminal = step.read.is_empty()
                        && step.search.is_empty()
                        && !step.act.is_empty()
                        && step
                            .act
                            .iter()
                            .all(|act| act.capability == "say" || act.capability == "wake");
                    pending_terminal.set(only_terminal);
                    Ok(step)
                },
                |call| {
                    let outcome = self.perform_call(&call, proactive);
                    if pending_terminal.get() {
                        if matches!(outcome, Outcome::Observed(_)) {
                            terminal_done.set(true);
                        } else {
                            pending_terminal.set(false);
                            terminal_done.set(false);
                        }
                    }
                    outcome
                },
            )
        };

        if let Err(err) = turn_result {
            self.emit_error(format!("执行回合失败: {err}"));
        }
        self.sync_config_if_changed();
        self.consolidate_locked(false);
        self.set_status("idle", "在线");
    }

    fn consolidate_locked(&self, force: bool) {
        let config = self.config.lock().unwrap().clone();
        let Some((provider, model)) = config.resolve_consolidate() else {
            return;
        };
        let provider = provider.clone();
        let model = model.to_string();
        let should_run = {
            let eja = self.eja.lock().unwrap();
            match eja.unconsolidated() {
                Ok(Some(window)) => force || window.entries.len() >= PAGE,
                _ => false,
            }
        };
        if !should_run {
            return;
        }
        self.set_status("consolidating", "整理记忆…");
        let res = self.eja.lock().unwrap().consolidate(2, |window| {
            self.brain
                .distill(&provider, &model, window)
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
        });
        if let Err(err) = res {
            self.emit_error(format!("整理记忆失败: {err}"));
        }
        self.set_status("idle", "在线");
    }

    fn perform_call(&self, call: &Call, proactive: bool) -> Outcome {
        match call.capability.as_str() {
            "say" => {
                let text = call
                    .input
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if text.is_empty() {
                    return Outcome::Failed("say 需要非空 text".into());
                }
                if proactive {
                    self.emit_notify("EJA", text);
                }
                Outcome::Observed("delivered".into())
            }
            "wake" => {
                let delay = call
                    .input
                    .get("delay_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let reason = call
                    .input
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if delay < 5 || reason.is_empty() {
                    return Outcome::Failed("wake 需要 delay_seconds >= 5 且非空 reason".into());
                }
                let fire_at = unix_now() + delay;
                let display = format_local_unix(fire_at);
                let alarm = WakeAlarm {
                    id: format!("w-{fire_at}-{}", call.id),
                    fire_at_unix: fire_at,
                    fire_at_display: display.clone(),
                    reason: reason.to_string(),
                };
                let mut wakes = self.wakes.lock().unwrap();
                wakes.push(alarm);
                wakes.sort_by_key(|a| a.fire_at_unix);
                save_wakes(&self.data_dir, &wakes);
                Outcome::Observed(format!("scheduled at {display} ({reason})"))
            }
            "shell" => {
                let command = call
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if command.is_empty() {
                    return Outcome::Failed("shell 需要非空 command".into());
                }
                let cwd = call
                    .input
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.data_dir.clone());
                run_shell(command, &cwd)
            }
            other if other.starts_with("mcp.") => {
                self.connectors.lock().unwrap().call(other, &call.input)
            }
            other => Outcome::Failed(format!("未知能力: {other}")),
        }
    }

    fn set_status(&self, status: &str, detail: &str) {
        *self.status.lock().unwrap() = (status.to_string(), detail.to_string());
        self.emit_state();
    }
}

fn run_shell(command: &str, cwd: &Path) -> Outcome {
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

    let deadline = Instant::now() + Duration::from_secs(30);
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
    let bounded = truncate_utf8(&combined, 6000);
    match status {
        Some(st) if st.success() => Outcome::Observed(bounded),
        Some(st) => Outcome::Failed(format!("exit {}: {bounded}", st.code().unwrap_or(-1))),
        None => Outcome::Failed(format!("命令超时 (30s): {bounded}")),
    }
}

fn open_subject(db_path: &Path) -> Result<Eja, String> {
    #[cfg(feature = "onnx")]
    if let Ok(model_dir) = std::env::var("EJA_ONNX_MODEL_DIR") {
        if Path::new(&model_dir).exists() {
            if let Ok(enc) = eja_core::OnnxEncoder::load(&model_dir) {
                return Eja::open_with_encoder(db_path, enc).map_err(|err| err.to_string());
            }
        }
    }
    Eja::open(db_path).map_err(|err| err.to_string())
}

fn load_recent_entries(eja: &Eja, limit: usize) -> Vec<Entry> {
    let Ok(view) = eja.view() else {
        return Vec::new();
    };
    if view.revision == 0 {
        return Vec::new();
    }
    let mut collected = Vec::new();
    let mut cursor = view.revision;
    while collected.len() < limit && cursor > 0 {
        let Ok(page) = eja.history(cursor) else { break };
        if page.is_empty() {
            break;
        }
        cursor = page
            .last()
            .map(|entry| entry.id.saturating_sub(1))
            .unwrap_or(0);
        collected.extend(page);
    }
    collected.truncate(limit);
    collected.reverse();
    collected
}

fn build_stream(entries: &[Entry]) -> Vec<StreamItem> {
    let mut returns: HashMap<Id, &Outcome> = HashMap::new();
    for entry in entries {
        if let Fact::Returned { action, outcome } = &entry.fact {
            returns.insert(*action, outcome);
        }
    }
    let mut items = Vec::new();
    let mut proactive_turn = false;
    for entry in entries {
        match &entry.fact {
            Fact::Noticed {
                source: Source::Person,
                text,
            } => {
                proactive_turn = false;
                items.push(StreamItem {
                    id: entry.id,
                    kind: "person".into(),
                    text: text.clone(),
                    detail: None,
                    proactive: None,
                });
            }
            Fact::Noticed {
                source: Source::Clock,
                text,
            } => {
                proactive_turn = true;
                items.push(StreamItem {
                    id: entry.id,
                    kind: "event".into(),
                    text: text.clone(),
                    detail: None,
                    proactive: None,
                });
            }
            Fact::Acted { capability, input } if capability == "say" => {
                if let Some(text) = input.get("text").and_then(|v| v.as_str()) {
                    items.push(StreamItem {
                        id: entry.id,
                        kind: "eja".into(),
                        text: text.to_string(),
                        detail: None,
                        proactive: if proactive_turn { Some(true) } else { None },
                    });
                }
            }
            Fact::Acted { capability, input } => {
                let outcome_text = returns.get(&entry.id).map(|out| match out {
                    Outcome::Observed(s) => s.clone(),
                    Outcome::Failed(s) => format!("失败: {s}"),
                    Outcome::Unknown(s) => format!("未知: {s}"),
                });
                let label = match capability.as_str() {
                    "wake" => format!(
                        "定时唤醒: {}",
                        input.get("reason").and_then(|v| v.as_str()).unwrap_or("")
                    ),
                    "shell" => format!(
                        "shell: {}",
                        truncate_utf8(
                            input.get("command").and_then(|v| v.as_str()).unwrap_or(""),
                            64
                        )
                    ),
                    other => other.to_string(),
                };
                items.push(StreamItem {
                    id: entry.id,
                    kind: "event".into(),
                    text: label,
                    detail: outcome_text.or_else(|| serde_json::to_string_pretty(input).ok()),
                    proactive: None,
                });
            }
            _ => {}
        }
    }
    items
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
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

fn current_local_time() -> String {
    Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M:%S %Z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| format!("unix:{}", unix_now()))
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
    use crate::config::{Assignments, ConnectorConfig, ConnectorTransport, ModelRef, ProviderKind};
    use crate::mcp::McpTool;
    use eja_core::{Act, Consolidation, View};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockBrain {
        judge_calls: AtomicUsize,
        distill_calls: AtomicUsize,
        mode: &'static str,
    }

    impl Brain for MockBrain {
        fn judge(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            view: &View,
            mcp_tools: &[McpTool],
            _has_encoder: bool,
            _local_time: &str,
            _data_dir: &str,
        ) -> Result<Step, String> {
            self.judge_calls.fetch_add(1, Ordering::SeqCst);
            if self.mode == "mcp" {
                let already_called = view.recent.iter().any(|e| {
                    matches!(&e.fact, Fact::Returned { outcome: Outcome::Observed(s), .. } if s.contains("pong:hello"))
                });
                if !already_called {
                    let cap = mcp_tools[0].capability.clone();
                    return Ok(Step {
                        act: vec![Act {
                            capability: cap,
                            input: json!({ "msg": "hello" }),
                        }],
                        ..Step::default()
                    });
                }
                return Ok(Step {
                    act: vec![Act {
                        capability: "say".into(),
                        input: json!({ "text": "mcp ok" }),
                    }],
                    ..Step::default()
                });
            }
            Ok(Step {
                act: vec![Act {
                    capability: "say".into(),
                    input: json!({ "text": "收到" }),
                }],
                ..Step::default()
            })
        }

        fn distill(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            window: &Consolidation,
        ) -> Result<String, String> {
            self.distill_calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!(
                "已整理 {} 条记录，用户叫 bey",
                window.entries.len()
            ))
        }

        fn test_connection(
            &self,
            _provider: &ProviderConfig,
            model: &str,
        ) -> Result<String, String> {
            Ok(format!("ok:{model}"))
        }
    }

    fn configured(connectors: Vec<ConnectorConfig>) -> HostConfig {
        HostConfig {
            providers: vec![ProviderConfig {
                id: "local".into(),
                name: "Local".into(),
                kind: ProviderKind::OpenAi,
                base_url: "http://127.0.0.1:11434/v1".into(),
                api_key: String::new(),
                models: vec!["mock-model".into()],
            }],
            assignments: Assignments {
                judge: ModelRef {
                    provider_id: "local".into(),
                    model: "mock-model".into(),
                },
                consolidate: None,
            },
            connectors,
        }
    }

    #[test]
    fn direct_core_turn_single_call_and_auto_consolidate() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            distill_calls: AtomicUsize::new(0),
            mode: "say",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));

        rt.handle_user_message("我叫 bey");
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);

        rt.handle_user_message("第 2 轮");
        rt.handle_user_message("第 3 轮");
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 3);
        assert!(brain.distill_calls.load(Ordering::SeqCst) >= 1);

        let snap = rt.snapshot();
        assert!(snap.consolidated.unwrap_or_default().contains("bey"));
    }

    #[test]
    fn mcp_stdio_connector_discover_and_invoke_in_turn() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("mock_mcp.py");
        fs::write(
            &script_path,
            r#"import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"Echo tool","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}},"required":["msg"]}}]}}), flush=True)
    elif method == "tools/call":
        arg = msg["params"]["arguments"].get("msg", "")
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"pong:" + arg}]}}), flush=True)
"#,
        )
        .unwrap();

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            distill_calls: AtomicUsize::new(0),
            mode: "mcp",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![ConnectorConfig {
            id: "demo".into(),
            name: "Demo MCP".into(),
            enabled: true,
            transport: ConnectorTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec![script_path.display().to_string()],
                env: BTreeMap::new(),
            },
        }]));

        let snap = rt.snapshot();
        assert_eq!(snap.connector_statuses.len(), 1);
        assert!(snap.connector_statuses[0].connected);
        assert_eq!(snap.connector_statuses[0].tools.len(), 1);
        assert_eq!(
            snap.connector_statuses[0].tools[0].capability,
            "mcp.demo.echo"
        );

        rt.handle_user_message("ping mcp");
        let after = rt.snapshot();
        assert!(after
            .stream
            .iter()
            .any(|item| item.kind == "event" && item.detail.as_deref() == Some("pong:hello")));
        assert!(after
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "mcp ok"));
    }
}
