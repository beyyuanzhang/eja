use crate::config::{HostConfig, ProviderConfig};
use crate::hands::{Hands, ToolSpec, WakeAlarm};
use crate::llm::{append_tool_results, project_window, Brain, HttpBrain, ToolCall};
use crate::mcp::{truncate_utf8, ConnectorStatus};
use crate::sandbox::Sandbox;
use eja_core::{Eja, Entry, Fact, Id, Outcome, Source};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub type EventSink = Arc<dyn Fn(Value) + Send + Sync>;
pub const PROTOCOL_VERSION: u32 = 1;
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    pub protocol_version: u32,
    pub kernel_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources_dir: Option<String>,
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
    core_dir: PathBuf,
    resources_dir: Option<PathBuf>,
    eja: Mutex<Eja>,
    config: Mutex<HostConfig>,
    connectors_bytes: Mutex<Option<Vec<u8>>>,
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
        let core_dir = data_dir.join("core");
        fs::create_dir_all(&core_dir).map_err(|err| err.to_string())?;
        let sandbox = Sandbox::new(&core_dir)?;
        let eja = open_subject(&core_dir.join("subject.db"))?;
        let resources_dir = resolve_resources_dir();
        sync_kernel_version(&core_dir, &eja, resources_dir.as_deref());
        let config = HostConfig::load(&core_dir, &data_dir);
        let connectors_bytes = read_connectors_bytes(&data_dir);
        let hands = Hands::open(
            data_dir.clone(),
            core_dir.clone(),
            sandbox,
            &config.connectors,
        );
        let anchor = load_anchor(&core_dir);
        Ok(Self {
            data_dir,
            core_dir,
            resources_dir,
            eja: Mutex::new(eja),
            config: Mutex::new(config),
            connectors_bytes: Mutex::new(connectors_bytes),
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
        let entries = eja.recent(80).unwrap_or_default();
        let concerns = eja
            .concerns(revision)
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
            protocol_version: PROTOCOL_VERSION,
            kernel_version: KERNEL_VERSION.to_string(),
            resources_dir: self
                .resources_dir
                .as_ref()
                .map(|dir| dir.display().to_string()),
            status,
            status_detail,
            data_dir: self.data_dir.display().to_string(),
            configured: config.is_configured(),
            config: config.redacted_for_ui(),
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

    pub fn save_config(&self, mut updated: HostConfig) {
        {
            let current = self.config.lock().unwrap();
            updated.merge_preserved(&current);
        }
        if let Err(err) = updated.save(&self.core_dir, &self.data_dir) {
            self.emit_error(format!("保存配置失败: {err}"));
            return;
        }
        let connectors_changed = {
            let mut guard = self.config.lock().unwrap();
            let changed = guard.connectors != updated.connectors;
            *guard = updated.clone();
            changed
        };
        *self.connectors_bytes.lock().unwrap() = read_connectors_bytes(&self.data_dir);
        if connectors_changed {
            self.hands.lock().unwrap().reload(&updated.connectors);
        }
        self.emit_state();
    }

    pub fn reload_connectors(&self) {
        match HostConfig::load_connectors(&self.data_dir) {
            Ok(connectors) => {
                self.config.lock().unwrap().connectors = connectors.clone();
                *self.connectors_bytes.lock().unwrap() = read_connectors_bytes(&self.data_dir);
                self.hands.lock().unwrap().reload(&connectors);
                self.emit_state();
            }
            Err(err) => self.emit_error(err),
        }
    }

    pub fn test_provider(&self, id: String, mut provider: ProviderConfig, model: String) {
        if provider.api_key.is_empty() {
            if let Some(existing) = self
                .config
                .lock()
                .unwrap()
                .providers
                .iter()
                .find(|p| p.id == provider.id)
            {
                provider.api_key = existing.api_key.clone();
            }
        }
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

    fn sync_connectors_if_changed(&self) {
        let latest_bytes = read_connectors_bytes(&self.data_dir);
        let changed = {
            let mut guard = self.connectors_bytes.lock().unwrap();
            if *guard != latest_bytes {
                *guard = latest_bytes;
                true
            } else {
                false
            }
        };
        if changed {
            match HostConfig::load_connectors(&self.data_dir) {
                Ok(connectors) => {
                    let connectors_changed = {
                        let mut cfg = self.config.lock().unwrap();
                        let diff = cfg.connectors != connectors;
                        cfg.connectors = connectors.clone();
                        diff
                    };
                    if connectors_changed {
                        self.hands.lock().unwrap().reload(&connectors);
                    }
                }
                Err(err) => self.emit_error(err),
            }
        }
    }

    fn run_turn_locked(&self, proactive: bool) {
        self.sync_connectors_if_changed();
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
        let resources_line = match &self.resources_dir {
            Some(dir) => format!("\n只读协议与模板参考：{}", dir.display()),
            None => String::new(),
        };
        let system = format!(
            "你是 EJA，一个持续存在的自我同一性主体。\n\
            内核版本：{KERNEL_VERSION}（协议 v{PROTOCOL_VERSION}）{resources_line}\n\
            数据目录：{}\n\
            连接器配置：{}/connectors.json（变更会自动热加载）\n\
            界面源码：{}/ui/（修改 SwiftUI 后运行 {}/ui/apply.sh 编译并热重启前端）\n\
            本地时区：{}\n\n\
            前置消息是你按时间正序排列的不可变主体日志；末尾新增的 `noticed` 记录是本次触发。不调工具时的文本回复会发送给人，留空则保持沉默。",
            data_dir_str, data_dir_str, data_dir_str, data_dir_str, self.local_tz
        );
        let mut messages = {
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
                save_anchor(&self.core_dir, *anchor);
            }
            projected
        };

        let mut completed = false;
        let mut fatal_error: Option<String> = None;

        for step_index in 0..max_steps {
            if step_index > 0 {
                self.sync_connectors_if_changed();
            }
            let tools = self.hands.lock().unwrap().tools();
            let reply = match self
                .brain
                .complete(&provider, &model, &system, &messages, &tools)
            {
                Ok(r) => r,
                Err(err) => {
                    fatal_error = Some(err);
                    break;
                }
            };

            if reply.calls.is_empty() {
                let text = reply.text.trim();
                if !text.is_empty() {
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

            messages.push(reply.message);
            let mut results = Vec::with_capacity(reply.calls.len());
            for call in reply.calls {
                match self.execute_tool_call(&call, &tools) {
                    Ok(val) => results.push((call.id, val)),
                    Err(err) => {
                        fatal_error = Some(err);
                        break;
                    }
                }
            }
            if fatal_error.is_some() {
                break;
            }
            append_tool_results(&provider.kind, &mut messages, results);
        }

        if let Some(err) = fatal_error {
            self.emit_error(format!("执行回合失败: {err}"));
        } else if !completed {
            self.emit_error(format!("达到单回合最大步数上限 ({max_steps})，回合已暂停"));
        }
        self.sync_connectors_if_changed();
        self.set_status("idle", "在线");
    }

    fn execute_tool_call(&self, call: &ToolCall, tools: &[ToolSpec]) -> Result<Value, String> {
        match call.name.as_str() {
            "keep" => {
                let text = call
                    .args
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let concern = call.args.get("concern").and_then(Value::as_i64);
                match self.eja.lock().unwrap().keep(concern, text) {
                    Ok(entry) => {
                        let Fact::Kept { concern, .. } = entry.fact else {
                            unreachable!()
                        };
                        Ok(json!({ "status": "kept", "concern": concern, "fact_id": entry.id }))
                    }
                    Err(eja_core::Error::Invalid(msg)) => Ok(json!({ "error": msg })),
                    Err(err) => Err(err.to_string()),
                }
            }
            "release" => match call.args.get("concern").and_then(Value::as_i64) {
                Some(id) => match self.eja.lock().unwrap().release(id) {
                    Ok(entry) => {
                        Ok(json!({ "status": "released", "concern": id, "fact_id": entry.id }))
                    }
                    Err(eja_core::Error::Invalid(msg)) => Ok(json!({ "error": msg })),
                    Err(err) => Err(err.to_string()),
                },
                None => Ok(json!({ "error": "release 需要整数 concern" })),
            },
            "read" => match call.args.get("fact").and_then(Value::as_i64) {
                Some(id) => match self.eja.lock().unwrap().evidence(id) {
                    Ok(ev) => Ok(json!(ev)),
                    Err(eja_core::Error::Invalid(msg)) => Ok(json!({ "error": msg })),
                    Err(err) => Err(err.to_string()),
                },
                None => Ok(json!({ "error": "read 需要整数 fact" })),
            },
            "search" => {
                let cue = call
                    .args
                    .get("cue")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                match self.eja.lock().unwrap().search(cue) {
                    Ok(matches) => Ok(json!({ "cue": cue, "matches": matches })),
                    Err(eja_core::Error::Invalid(msg)) => Ok(json!({ "error": msg })),
                    Err(err) => Err(err.to_string()),
                }
            }
            other => {
                let Some(spec) = tools.iter().find(|t| t.name == other) else {
                    return Ok(json!({ "error": format!("未知工具: {other}") }));
                };
                let grants: Vec<&str> = tools.iter().map(|t| t.capability.as_str()).collect();
                let acted = {
                    let eja = self.eja.lock().unwrap();
                    let revision = eja.revision().map_err(|err| err.to_string())?;
                    eja.act(revision, &spec.capability, call.args.clone(), &grants)
                };
                let acted = match acted {
                    Ok(c) => c,
                    Err(eja_core::Error::Invalid(msg)) => return Ok(json!({ "error": msg })),
                    Err(err) => return Err(err.to_string()),
                };
                let outcome = self.hands.lock().unwrap().execute(&acted);
                let receipt = self
                    .eja
                    .lock()
                    .unwrap()
                    .finish(acted.id, outcome)
                    .map_err(|err| err.to_string())?;
                let Fact::Returned { outcome, .. } = receipt.fact else {
                    unreachable!()
                };
                Ok(json!({ "id": receipt.id, "action": acted.id, "outcome": outcome }))
            }
        }
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

fn read_connectors_bytes(data_dir: &Path) -> Option<Vec<u8>> {
    fs::read(HostConfig::connectors_path(data_dir)).ok()
}

fn load_anchor(core_dir: &Path) -> Id {
    fs::read(core_dir.join("window.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<WindowState>(&bytes).ok())
        .map(|state| state.anchor.max(0))
        .unwrap_or(0)
}

fn save_anchor(core_dir: &Path, anchor: Id) {
    let state = WindowState { anchor };
    if let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = fs::write(core_dir.join("window.json"), bytes);
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

fn resolve_resources_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("EJA_RESOURCES_DIR") {
        let dir = PathBuf::from(path);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let resources = macos_dir.parent()?.join("Resources");
    resources.is_dir().then_some(resources)
}

fn sync_kernel_version(core_dir: &Path, eja: &Eja, resources_dir: Option<&Path>) {
    let version_path = core_dir.join("kernel_version");
    let prev = fs::read_to_string(&version_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(old) = prev.as_deref() {
        if old == KERNEL_VERSION {
            return;
        }
        let notice = match resources_dir {
            Some(dir) => format!(
                "内核已从 {old} 升级到 {KERNEL_VERSION}（协议 v{PROTOCOL_VERSION}）。协议文档与默认界面参考位于 {}，可按需对照调整本地前端或连接器。",
                dir.display()
            ),
            None => format!("内核已从 {old} 升级到 {KERNEL_VERSION}（协议 v{PROTOCOL_VERSION}）。"),
        };
        if eja.notice(Source::World, notice).is_err() {
            return;
        }
    }
    let tmp = core_dir.join("kernel_version.tmp");
    if fs::write(&tmp, format!("{KERNEL_VERSION}\n")).is_ok() {
        let _ = fs::rename(&tmp, &version_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectorConfig, ConnectorTransport, ModelRef, ProviderKind};
    use crate::llm::ModelReply;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn text_reply(text: &str) -> ModelReply {
        ModelReply {
            message: json!({ "role": "assistant", "content": text }),
            text: text.into(),
            calls: Vec::new(),
        }
    }

    fn tool_reply(id: &str, name: &str, args: Value) -> ModelReply {
        ModelReply {
            message: json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }]
            }),
            text: String::new(),
            calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                args,
            }],
        }
    }

    struct MockBrain {
        judge_calls: AtomicUsize,
        mode: &'static str,
    }

    impl Brain for MockBrain {
        fn complete(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            _system: &str,
            messages: &[Value],
            tools: &[ToolSpec],
        ) -> Result<ModelReply, String> {
            self.judge_calls.fetch_add(1, Ordering::SeqCst);
            if self.mode == "mcp" {
                let already_called = messages.iter().any(|m| {
                    m["role"] == "tool"
                        && m["content"].as_str().unwrap_or("").contains("pong:hello")
                });
                if !already_called {
                    let name = tools
                        .iter()
                        .find(|t| t.capability.starts_with("mcp."))
                        .unwrap()
                        .name
                        .clone();
                    return Ok(tool_reply("c1", &name, json!({ "msg": "hello" })));
                }
                return Ok(text_reply("mcp ok"));
            }
            Ok(text_reply("你好，我在。"))
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
                has_api_key: false,
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
        fn complete(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            _system: &str,
            messages: &[Value],
            _tools: &[ToolSpec],
        ) -> Result<ModelReply, String> {
            if messages.last().is_some_and(|m| m["role"] == "tool") {
                return Ok(text_reply("收到"));
            }
            let mut windows = self.windows.lock().unwrap();
            let first_turn = windows.is_empty();
            windows.push(messages.to_vec());
            if first_turn {
                return Ok(tool_reply("k1", "keep", json!({ "text": "长期挂念" })));
            }
            Ok(text_reply("收到"))
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
        fn complete(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            _system: &str,
            messages: &[Value],
            _tools: &[ToolSpec],
        ) -> Result<ModelReply, String> {
            if let Some(tool_msg) = messages.last().filter(|m| m["role"] == "tool") {
                let parsed: Value =
                    serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
                if let Some(err) = parsed.get("error").and_then(Value::as_str) {
                    *self.seen_error.lock().unwrap() = Some(err.to_string());
                    return Ok(text_reply("已自行纠正"));
                }
            }
            Ok(tool_reply("r1", "release", json!({ "concern": 99999 })))
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

    #[test]
    fn split_config_and_connectors_and_preserve_state_on_invalid_connectors_json() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));

        let core_config = HostConfig::config_path(&dir.path().join("core"));
        let connectors_path = HostConfig::connectors_path(dir.path());
        assert!(core_config.exists());
        assert!(connectors_path.exists());
        assert!(!dir.path().join("config.json").exists());
        assert!(!fs::read_to_string(&core_config)
            .unwrap()
            .contains("connectors"));

        fs::write(&connectors_path, "{ broken json").unwrap();
        rt.handle_user_message("依然可用");

        let snap = rt.snapshot();
        assert!(snap.configured);
        assert_eq!(snap.config.judge.provider_id, "local");
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "你好，我在。"));
    }

    #[test]
    fn redacts_api_key_in_state_and_preserves_it_on_empty_save() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt = HostRuntime::open_with_brain(dir.path().to_path_buf(), brain, Arc::new(|_| {}))
            .unwrap();
        let mut cfg = configured(vec![]);
        cfg.providers[0].api_key = "sk-top-secret".into();
        rt.save_config(cfg);

        let snap = rt.snapshot();
        assert_eq!(snap.protocol_version, PROTOCOL_VERSION);
        assert_eq!(snap.kernel_version, KERNEL_VERSION);
        assert_eq!(snap.config.providers[0].api_key, "");
        assert!(snap.config.providers[0].has_api_key);

        // Saving redacted config back from UI must preserve stored api_key on disk.
        rt.save_config(snap.config);
        let disk = fs::read_to_string(HostConfig::config_path(&dir.path().join("core"))).unwrap();
        assert!(disk.contains("sk-top-secret"));
        assert!(!disk.contains("has_api_key"));
    }

    #[test]
    fn detects_kernel_upgrade_and_emits_world_notice_once() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        assert!(rt.snapshot().stream.is_empty());
        let version_file = dir.path().join("core/kernel_version");
        assert_eq!(
            fs::read_to_string(&version_file).unwrap().trim(),
            KERNEL_VERSION
        );
        drop(rt);

        fs::write(&version_file, "0.9.0\n").unwrap();
        let upgraded =
            HostRuntime::open_with_brain(dir.path().to_path_buf(), brain, Arc::new(|_| {}))
                .unwrap();
        assert_eq!(
            fs::read_to_string(&version_file).unwrap().trim(),
            KERNEL_VERSION
        );
        assert!(upgraded
            .snapshot()
            .stream
            .iter()
            .any(|item| item.text.contains("0.9.0") && item.text.contains(KERNEL_VERSION)));
    }
}
