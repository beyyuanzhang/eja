use crate::config::{HostConfig, ProviderConfig};
use crate::hands::{Hands, WakeAlarm};
use crate::llm::{
    concerns_at, entries_since, project_window, Brain, HttpBrain, StepFeedback, TurnContext,
    TurnSession,
};
use crate::mcp::{truncate_utf8, ConnectorStatus};
use eja_core::{Eja, Entry, Fact, Id, Outcome, SearchHits, Source};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub type EventSink = Arc<dyn Fn(Value) + Send + Sync>;

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
    pub configured: bool,
    pub config: HostConfig,
    pub connector_statuses: Vec<ConnectorStatus>,
    pub stream: Vec<StreamItem>,
    pub concerns: Vec<ConcernDto>,
    pub wakes: Vec<WakeAlarm>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WindowState {
    anchor: Id,
}

pub struct HostRuntime {
    data_dir: PathBuf,
    eja: Mutex<Eja>,
    config: Mutex<HostConfig>,
    config_bytes: Mutex<Option<Vec<u8>>>,
    hands: Mutex<Hands>,
    anchor: Mutex<Id>,
    local_tz: String,
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
        let config_bytes = read_config_bytes(&data_dir);
        let hands = Hands::open(data_dir.clone(), &config.connectors);
        let anchor = load_anchor(&data_dir);
        Ok(Self {
            data_dir,
            eja: Mutex::new(eja),
            config: Mutex::new(config),
            config_bytes: Mutex::new(config_bytes),
            hands: Mutex::new(hands),
            anchor: Mutex::new(anchor),
            local_tz: detect_local_tz(),
            status: Mutex::new(("idle".into(), "在线".into())),
            busy: Mutex::new(()),
            brain,
            emit,
        })
    }

    pub fn snapshot(&self) -> HostState {
        let config = self.config.lock().unwrap().clone();
        let (status, status_detail) = self.status.lock().unwrap().clone();
        let (connector_statuses, wakes) = {
            let hands = self.hands.lock().unwrap();
            (hands.connector_statuses(), hands.wakes())
        };
        let eja = self.eja.lock().unwrap();
        let revision = eja.revision().unwrap_or(0);
        let entries = load_recent_entries(&eja, 80);
        let concerns = concerns_at(&eja, revision)
            .map(|items| {
                items
                    .iter()
                    .map(|c| ConcernDto {
                        id: c.id,
                        text: c.text.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        HostState {
            status,
            status_detail,
            data_dir: self.data_dir.display().to_string(),
            configured: config.is_configured(),
            config,
            connector_statuses,
            stream: build_stream(&entries),
            concerns,
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
        *self.config_bytes.lock().unwrap() = read_config_bytes(&self.data_dir);
        if connectors_changed {
            self.hands.lock().unwrap().reload(&updated.connectors);
        }
        self.emit_state();
    }

    pub fn reload_connectors(&self) {
        let config = HostConfig::load(&self.data_dir);
        *self.config.lock().unwrap() = config.clone();
        *self.config_bytes.lock().unwrap() = read_config_bytes(&self.data_dir);
        self.hands.lock().unwrap().reload(&config.connectors);
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
        let due = self.hands.lock().unwrap().drain_due_wakes();
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

    fn sync_config_if_changed(&self) {
        let latest_bytes = read_config_bytes(&self.data_dir);
        let changed = {
            let mut guard = self.config_bytes.lock().unwrap();
            if *guard != latest_bytes {
                *guard = latest_bytes;
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
                self.hands.lock().unwrap().reload(&loaded.connectors);
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
        let window_max_bytes = config.window_max_bytes();
        let max_steps = config.max_steps();

        self.set_status("thinking", "思考中…");
        let data_dir_str = self.data_dir.display().to_string();
        let window = {
            let eja = self.eja.lock().unwrap();
            let mut anchor = self.anchor.lock().unwrap();
            let initial_anchor = *anchor;
            let projected = match project_window(&eja, &mut anchor, window_max_bytes) {
                Ok(records) => records,
                Err(err) => {
                    drop(anchor);
                    drop(eja);
                    self.emit_error(format!("构建日志窗口失败: {err}"));
                    self.set_status("idle", "在线");
                    return;
                }
            };
            if *anchor != initial_anchor {
                save_anchor(&self.data_dir, *anchor);
            }
            projected
        };

        let mut turn = TurnSession::default();
        let mut previous_revision: Option<Id> = None;
        let mut step_actions: Vec<Id> = Vec::new();
        let mut step_reads: Vec<Result<eja_core::Evidence, String>> = Vec::new();
        let mut step_searches: Vec<Result<SearchHits, String>> = Vec::new();
        let mut commit_error: Option<String> = None;
        let mut completed = false;
        let mut fatal_error: Option<String> = None;

        for step_index in 0..max_steps {
            if step_index > 0 {
                self.sync_config_if_changed();
            }
            let tools = self.hands.lock().unwrap().tools();
            let (revision, entries) = {
                let eja = self.eja.lock().unwrap();
                let rev = match eja.revision() {
                    Ok(r) => r,
                    Err(err) => {
                        fatal_error = Some(err.to_string());
                        break;
                    }
                };
                if rev == 0 {
                    completed = true;
                    break;
                }
                let delta = match previous_revision.replace(rev) {
                    Some(previous) => match entries_since(&eja, previous, rev) {
                        Ok(items) => items,
                        Err(err) => {
                            fatal_error = Some(err.to_string());
                            break;
                        }
                    },
                    None => Vec::new(),
                };
                (rev, delta)
            };

            let ctx = TurnContext {
                provider: &provider,
                model: &model,
                tools: &tools,
                data_dir: &data_dir_str,
                local_tz: &self.local_tz,
                window: &window,
            };
            let decision = match self.brain.judge(
                &ctx,
                &mut turn,
                StepFeedback {
                    entries: &entries,
                    actions: &step_actions,
                    reads: &step_reads,
                    searches: &step_searches,
                    commit_error: commit_error.as_deref(),
                },
            ) {
                Ok(d) => d,
                Err(err) => {
                    fatal_error = Some(err);
                    break;
                }
            };

            step_actions.clear();
            step_reads.clear();
            step_searches.clear();
            commit_error = None;

            let step = decision.step;
            if step.is_empty() && !turn.has_pending() {
                if let Some(text) = decision
                    .reply
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if let Err(err) = self.eja.lock().unwrap().say(text) {
                        fatal_error = Some(err.to_string());
                        break;
                    }
                    if proactive {
                        (self.emit)(json!({ "event": "notify", "title": "EJA", "body": text }));
                    }
                }
                completed = true;
                break;
            }

            let grant_refs: Vec<&str> = tools.iter().map(|t| t.capability.as_str()).collect();
            let calls = if step.has_mutations() {
                let commit_res = self
                    .eja
                    .lock()
                    .unwrap()
                    .commit(revision, &step, &grant_refs);
                match commit_res {
                    Ok(calls) => calls,
                    Err(eja_core::Error::Invalid(msg)) => {
                        commit_error = Some(msg);
                        Vec::new()
                    }
                    Err(err) => {
                        fatal_error = Some(err.to_string());
                        break;
                    }
                }
            } else {
                Vec::new()
            };

            for call in calls {
                let id = call.id;
                step_actions.push(id);
                let outcome = self.hands.lock().unwrap().execute(&call);
                if let Err(err) = self.eja.lock().unwrap().finish(id, outcome) {
                    fatal_error = Some(err.to_string());
                    break;
                }
            }
            if fatal_error.is_some() {
                break;
            }

            if !step.read.is_empty() || !step.search.is_empty() {
                let eja = self.eja.lock().unwrap();
                let head = match eja.revision() {
                    Ok(h) => h,
                    Err(err) => {
                        fatal_error = Some(err.to_string());
                        break;
                    }
                };
                for fact_id in step.read {
                    if fact_id < 1 || fact_id > head {
                        step_reads.push(Err(format!(
                            "fact id {fact_id} 超出当前日志范围 (1..={head})"
                        )));
                    } else {
                        step_reads.push(eja.evidence(fact_id, head).map_err(|err| err.to_string()));
                    }
                }
                for cue in step.search {
                    let res = eja
                        .search(&cue)
                        .map(|matches| SearchHits { cue, matches })
                        .map_err(|err| err.to_string());
                    step_searches.push(res);
                }
            }
        }

        if let Some(err) = fatal_error {
            self.emit_error(format!("执行回合失败: {err}"));
        } else if !completed {
            self.emit_error(format!("达到单回合最大步数上限 ({max_steps})，回合已暂停"));
        }
        self.sync_config_if_changed();
        self.set_status("idle", "在线");
    }

    fn set_status(&self, status: &str, detail: &str) {
        *self.status.lock().unwrap() = (status.to_string(), detail.to_string());
        self.emit_state();
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
    let Ok(revision) = eja.revision() else {
        return Vec::new();
    };
    if revision == 0 {
        return Vec::new();
    }
    let mut collected = Vec::new();
    let mut cursor = revision;
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
            Fact::Noticed {
                source: Source::World,
                text,
            } => {
                items.push(StreamItem {
                    id: entry.id,
                    kind: "event".into(),
                    text: text.clone(),
                    detail: None,
                    proactive: None,
                });
            }
            Fact::Said { text } => {
                items.push(StreamItem {
                    id: entry.id,
                    kind: "eja".into(),
                    text: text.clone(),
                    detail: None,
                    proactive: if proactive_turn { Some(true) } else { None },
                });
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

fn read_config_bytes(data_dir: &Path) -> Option<Vec<u8>> {
    fs::read(HostConfig::path(data_dir)).ok()
}

fn load_anchor(data_dir: &Path) -> Id {
    fs::read(data_dir.join("window.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<WindowState>(&bytes).ok())
        .map(|state| state.anchor.max(0))
        .unwrap_or(0)
}

fn save_anchor(data_dir: &Path, anchor: Id) {
    let state = WindowState { anchor };
    if let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = fs::write(data_dir.join("window.json"), bytes);
    }
}

fn detect_local_tz() -> String {
    Command::new("/bin/date")
        .arg("+%Z (%z)")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UTC (+0000)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectorConfig, ConnectorTransport, ModelRef, ProviderKind};
    use crate::llm::Decision;
    use eja_core::{Act, Keep, Step};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockBrain {
        judge_calls: AtomicUsize,
        mode: &'static str,
    }

    impl Brain for MockBrain {
        fn judge(
            &self,
            ctx: &TurnContext<'_>,
            _turn: &mut TurnSession,
            feedback: StepFeedback<'_>,
        ) -> Result<Decision, String> {
            self.judge_calls.fetch_add(1, Ordering::SeqCst);
            if self.mode == "mcp" {
                let already_called = feedback.entries.iter().any(|e| {
                    matches!(&e.fact, Fact::Returned { outcome: Outcome::Observed(s), .. } if s.contains("pong:hello"))
                });
                if !already_called {
                    let cap = ctx
                        .tools
                        .iter()
                        .find(|t| t.capability.starts_with("mcp."))
                        .unwrap()
                        .capability
                        .clone();
                    return Ok(Decision {
                        step: Step {
                            act: vec![Act {
                                capability: cap,
                                input: json!({ "msg": "hello" }),
                            }],
                            ..Step::default()
                        },
                        reply: None,
                    });
                }
                return Ok(Decision {
                    step: Step::default(),
                    reply: Some("mcp ok".into()),
                });
            }
            Ok(Decision {
                step: Step::default(),
                reply: Some("你好，我在。".into()),
            })
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
                base_url: "http://127.0.0.1:9999/v1".into(),
                api_key: String::new(),
                models: vec!["demo".into()],
            }],
            judge: ModelRef {
                provider_id: "local".into(),
                model: "demo".into(),
            },
            connectors,
            window_max_bytes: None,
            max_steps: None,
        }
    }

    #[test]
    fn user_turn_records_stream_and_runs_multiple_turns() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));
        rt.handle_user_message("你好");

        let snap = rt.snapshot();
        assert!(snap.configured);
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "person" && item.text == "你好"));
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "你好，我在。"));
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);

        rt.handle_user_message("第 2 轮");
        rt.handle_user_message("第 3 轮");
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 3);
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

    struct WindowRecordingBrain {
        windows: Mutex<Vec<Vec<Value>>>,
    }

    impl Brain for WindowRecordingBrain {
        fn judge(
            &self,
            ctx: &TurnContext<'_>,
            _turn: &mut TurnSession,
            feedback: StepFeedback<'_>,
        ) -> Result<Decision, String> {
            if !feedback.entries.is_empty() {
                return Ok(Decision {
                    step: Step::default(),
                    reply: Some("收到".into()),
                });
            }
            let mut windows = self.windows.lock().unwrap();
            let first_turn = windows.is_empty();
            windows.push(ctx.window.to_vec());
            if first_turn {
                return Ok(Decision {
                    step: Step {
                        keep: vec![Keep {
                            concern: None,
                            text: "长期挂念".into(),
                        }],
                        ..Step::default()
                    },
                    reply: None,
                });
            }
            Ok(Decision {
                step: Step::default(),
                reply: Some("收到".into()),
            })
        }

        fn test_connection(
            &self,
            _provider: &ProviderConfig,
            model: &str,
        ) -> Result<String, String> {
            Ok(format!("ok:{model}"))
        }
    }

    #[test]
    fn window_stays_append_only_across_turns_and_persists_anchor_on_reset() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(WindowRecordingBrain {
            windows: Mutex::new(Vec::new()),
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        let mut cfg = configured(vec![]);
        cfg.window_max_bytes = Some(900);
        rt.save_config(cfg);

        rt.handle_user_message("第一轮");
        rt.handle_user_message("第二轮");

        {
            let windows = brain.windows.lock().unwrap();
            assert_eq!(windows.len(), 2);
            assert!(windows[1].starts_with(&windows[0]));
        }

        // Exceed the window budget so anchor advances and persists to window.json.
        rt.handle_user_message(&"长消息".repeat(120));
        rt.handle_user_message("推进窗口");
        drop(rt);

        // Reopen runtime and verify persisted anchor starts cleanly with the open concern snapshot.
        let restarted =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        restarted.handle_user_message("重启后第一轮");

        let windows = brain.windows.lock().unwrap();
        let last = windows.last().unwrap();
        let first_line: Value = serde_json::from_str(last[0]["content"].as_str().unwrap()).unwrap();
        assert!(first_line["anchor"].as_i64().unwrap() > 0);
        assert_eq!(first_line["concerns"][0]["text"], "长期挂念");
    }

    struct SelfHealingBrain {
        seen_error: Mutex<Option<String>>,
    }

    impl Brain for SelfHealingBrain {
        fn judge(
            &self,
            _ctx: &TurnContext<'_>,
            _turn: &mut TurnSession,
            feedback: StepFeedback<'_>,
        ) -> Result<Decision, String> {
            if let Some(err) = feedback.commit_error {
                *self.seen_error.lock().unwrap() = Some(err.to_string());
                return Ok(Decision {
                    step: Step::default(),
                    reply: Some("已自行纠正".into()),
                });
            }
            Ok(Decision {
                step: Step {
                    release: vec![99999],
                    ..Step::default()
                },
                reply: None,
            })
        }

        fn test_connection(
            &self,
            _provider: &ProviderConfig,
            model: &str,
        ) -> Result<String, String> {
            Ok(format!("ok:{model}"))
        }
    }

    #[test]
    fn invalid_commit_feeds_error_back_instead_of_aborting_turn() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(SelfHealingBrain {
            seen_error: Mutex::new(None),
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));

        rt.handle_user_message("测试自纠");
        assert!(brain.seen_error.lock().unwrap().is_some());
        let snap = rt.snapshot();
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "已自行纠正"));
    }
}
