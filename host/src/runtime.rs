use crate::config::{
    write_json_atomic, CapabilityConfig, CapabilityTransport, ConfigPatch, HostConfig,
    ProviderConfig,
};
use crate::hands::{activity_label, format_local_unix, Hands, ToolSpec, ToolTarget};
use crate::http::truncate_utf8;
use crate::llm::{
    apply_tool_exchange, expire_tool_images, list_models, project_window_through, tool_messages,
    Brain, HttpBrain, ToolCall, ToolOutput,
};
use crate::mcp::CapabilityStatus;
use crate::oauth::{spawn_oauth_flow, OAuthEvent};
use crate::sandbox::{Sandbox, TrustedDir};
use crate::state::{self, WakeAlarm};
use eja_core::{Eja, Entry, Fact, Id, Outcome, Source, Timestamp};
use eja_memory::MemoryIndex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::time::{Duration, Instant};

pub type EventSink = Arc<dyn Fn(Value) + Send + Sync>;
pub const PROTOCOL_VERSION: u32 = 2;
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_RETRY_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    AlreadyRunning,
    Failed(String),
}

impl From<String> for OpenError {
    fn from(msg: String) -> Self {
        Self::Failed(msg)
    }
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum HostCommand {
    Send {
        text: String,
    },
    SaveConfig {
        config: ConfigPatch,
        id: Option<String>,
    },
    TestProvider {
        id: String,
        provider: ProviderConfig,
        model: String,
    },
    ListModels {
        id: String,
        provider: ProviderConfig,
    },
    OauthCapability {
        id: String,
    },
    ReloadCapabilities,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamItem {
    pub id: Id,
    pub kind: String, // "person" | "eja" | "event"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proactive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
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
    pub capability_statuses: Vec<CapabilityStatus>,
    pub stream: Vec<StreamItem>,
    pub concerns: Vec<ConcernDto>,
    pub wakes: Vec<WakeAlarm>,
}

pub struct HostRuntime {
    data_dir: TrustedDir,
    core_dir: TrustedDir,
    resources_dir: Option<PathBuf>,
    _instance_lock: fs::File,
    eja: Eja,
    memory: MemoryIndex,
    config: HostConfig,
    capabilities_bytes: Option<Vec<u8>>,
    hands: Hands,
    oauth_tx: Sender<OAuthEvent>,
    oauth_rx: Receiver<OAuthEvent>,
    oauth_cancels: HashMap<String, Arc<AtomicBool>>,
    inbox: Option<Receiver<Result<HostCommand, String>>>,
    deferred: VecDeque<Result<HostCommand, String>>,
    anchor: Id,
    retry_at: Option<Instant>,
    retry_attempts: u8,
    local_tz: String,
    status: (String, String),
    brain: Arc<dyn Brain>,
    emit: EventSink,
}

fn needs_handshake(capability: &CapabilityConfig) -> bool {
    capability.enabled && capability.transport.is_mcp()
}

impl HostRuntime {
    pub fn open(data_dir: PathBuf, emit: EventSink) -> Result<Self, OpenError> {
        Self::open_with_brain(data_dir, Arc::new(HttpBrain), emit)
    }

    pub fn open_with_brain(
        data_dir: PathBuf,
        brain: Arc<dyn Brain>,
        emit: EventSink,
    ) -> Result<Self, OpenError> {
        fs::create_dir_all(&data_dir).map_err(|err| err.to_string())?;
        let data_path = fs::canonicalize(data_dir).map_err(|err| err.to_string())?;
        let data_dir =
            TrustedDir::open(data_path).map_err(|err| format!("锁定数据目录失败: {err}"))?;
        let name = data_dir
            .path()
            .file_name()
            .ok_or_else(|| OpenError::Failed("数据目录不能是文件系统根目录".into()))?;
        let mut core_name = name.to_os_string();
        if name == "Eja" {
            core_name = "ai.eja.kernel".into();
        } else {
            core_name.push(".kernel");
        }
        let core_path = data_dir.path().with_file_name(core_name);
        let instance_lock = match lock_core_dir(&core_path) {
            Ok(lock) => lock,
            Err(LockError::AlreadyRunning(ui_pid)) => {
                (emit)(json!({ "event": "already_running", "ui_pid": ui_pid }));
                return Err(OpenError::AlreadyRunning);
            }
            Err(LockError::Io(msg)) => return Err(OpenError::Failed(msg)),
        };
        let core_dir =
            TrustedDir::open(core_path).map_err(|err| format!("锁定内核目录失败: {err}"))?;
        let sandbox = Sandbox::new(core_dir.path())?;
        let eja = Eja::open(core_dir.path().join("subject.db")).map_err(|err| err.to_string())?;
        let memory = open_memory(&core_dir.path().join("memory.db"))?;
        let resources_dir = resolve_resources_dir();
        let anchor = state::bootstrap(
            &eja,
            resources_dir.as_deref(),
            KERNEL_VERSION,
            PROTOCOL_VERSION,
        )?;
        let mut config = HostConfig::load(&core_dir);
        let capabilities_bytes = HostConfig::read_capabilities(&data_dir).unwrap_or_else(|err| {
            (emit)(json!({"event":"error", "message":err}));
            None
        });
        config.capabilities = HostConfig::parse_capabilities(capabilities_bytes.as_deref())
            .unwrap_or_else(|err| {
                (emit)(json!({"event":"error", "message":err}));
                vec![CapabilityConfig::builtin_shell()]
            });
        let status = if config.capabilities.iter().any(needs_handshake) {
            ("connecting".into(), "连接能力…".into())
        } else {
            ("idle".into(), "在线".into())
        };
        let hands = Hands::open(
            data_dir.clone(),
            core_dir.clone(),
            sandbox,
            &config.capabilities,
        );
        let (oauth_tx, oauth_rx) = mpsc::channel();
        Ok(Self {
            data_dir,
            core_dir,
            resources_dir,
            _instance_lock: instance_lock,
            eja,
            memory,
            config,
            capabilities_bytes,
            hands,
            oauth_tx,
            oauth_rx,
            oauth_cancels: HashMap::new(),
            inbox: None,
            deferred: VecDeque::new(),
            anchor,
            retry_at: None,
            retry_attempts: 0,
            local_tz: detect_local_tz(),
            status,
            brain,
            emit,
        })
    }

    pub fn connect_external_capabilities(&mut self) {
        if self.config.capabilities.iter().any(needs_handshake) {
            self.hands.reload(&self.config.capabilities, false);
            self.set_status("idle", "在线");
        }
    }

    pub fn run(&mut self, inbox: Receiver<Result<HostCommand, String>>) -> Result<(), String> {
        self.inbox = Some(inbox);
        self.emit_state(None);
        self.connect_external_capabilities();
        self.drain_turn_commands();
        self.process_pending_turn();
        loop {
            let delay = self.next_timer_delay()?;
            if delay.is_some_and(|delay| delay.is_zero()) {
                self.tick_timers()?;
                continue;
            }
            let command = if let Some(command) = self.deferred.pop_front() {
                command
            } else {
                let inbox = self.inbox.as_ref().expect("host command inbox");
                let received = match delay {
                    Some(delay) => inbox.recv_timeout(delay),
                    None => inbox
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match received {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.tick_timers()?;
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            };
            match command {
                Ok(HostCommand::Send { text }) => self.handle_user_message(&text),
                Ok(HostCommand::SaveConfig { config, id }) => {
                    self.save_config_request(config, id, true)
                }
                Ok(HostCommand::TestProvider {
                    id,
                    provider,
                    model,
                }) => self.test_provider(id, provider, model),
                Ok(HostCommand::ListModels { id, provider }) => {
                    self.list_provider_models(id, provider)
                }
                Ok(HostCommand::OauthCapability { id }) => self.oauth_capability(id),
                Ok(HostCommand::ReloadCapabilities) => self.reload_capabilities(),
                Err(err) => self.emit_error(err),
            }
        }
    }

    fn drain_turn_commands(&mut self) {
        while let Some(command) = self.inbox.as_ref().and_then(|inbox| inbox.try_recv().ok()) {
            match command {
                Ok(HostCommand::SaveConfig { config, id }) => {
                    self.save_config_request(config, id, false)
                }
                Ok(HostCommand::ReloadCapabilities) => self.reload_capabilities(),
                command => self.deferred.push_back(command),
            }
        }
    }

    fn next_wake_delay(&self) -> Result<Option<Duration>, String> {
        let now = self
            .eja
            .now()
            .unwrap_or_else(|_| Timestamp::now())
            .as_unix();
        state::next_wake_at(&self.eja)
            .map(|at| at.map(|at| Duration::from_secs(at.saturating_sub(now).max(0) as u64)))
            .map_err(|err| err.to_string())
    }

    pub fn next_timer_delay(&self) -> Result<Option<Duration>, String> {
        let wake = self.next_wake_delay()?;
        let retry = self
            .retry_at
            .map(|at| at.saturating_duration_since(Instant::now()));
        let base = match (wake, retry) {
            (Some(wake_delay), Some(retry_delay)) => Some(wake_delay.min(retry_delay)),
            (wake_delay, retry_delay) => wake_delay.or(retry_delay),
        };
        if !self.oauth_cancels.is_empty() {
            let poll = Duration::from_millis(100);
            return Ok(Some(base.map_or(poll, |d| d.min(poll))));
        }
        Ok(base)
    }

    fn clear_retry(&mut self) {
        self.retry_at = None;
        self.retry_attempts = 0;
    }

    #[cfg(test)]
    fn has_pending_trigger(&self) -> bool {
        state::next_trigger(&self.eja).unwrap().is_some()
    }

    pub fn process_pending_turn(&mut self) {
        loop {
            match state::next_trigger(&self.eja) {
                Ok(Some((id, source))) => {
                    if !self.run_turn(source != Source::Person, id) {
                        return;
                    }
                }
                Ok(None) => return,
                Err(err) => return self.emit_error(format!("检查待处理触发失败: {err}")),
            }
        }
    }

    fn persist_anchor(&mut self, next_anchor: Id) -> Result<(), String> {
        if next_anchor != self.anchor {
            state::save_anchor(&self.eja, next_anchor)
                .map_err(|err| format!("保存窗口状态失败: {err}"))?;
            self.anchor = next_anchor;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> HostState {
        let revision = self.eja.revision().unwrap_or(0);
        let entries = self.eja.recent(80).unwrap_or_default();
        let concerns = self
            .eja
            .concerns(revision)
            .map(|items| {
                items
                    .iter()
                    .map(|concern| ConcernDto {
                        id: concern.id,
                        text: concern.text.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let proactive_replies = state::proactive_reply_ids(&self.eja);
        HostState {
            protocol_version: PROTOCOL_VERSION,
            kernel_version: KERNEL_VERSION.to_string(),
            resources_dir: self
                .resources_dir
                .as_ref()
                .map(|dir| dir.display().to_string()),
            status: self.status.0.clone(),
            status_detail: self.status.1.clone(),
            data_dir: self.data_dir.path().display().to_string(),
            configured: self.config.is_configured(),
            config: self.config.redacted_for_ui(),
            capability_statuses: self.hands.capability_statuses(),
            stream: build_stream(&entries, &proactive_replies),
            concerns,
            wakes: state::pending_wakes(&self.eja).unwrap_or_default(),
        }
    }

    fn emit_state(&self, save_id: Option<String>) {
        if let Ok(mut payload) = serde_json::to_value(self.snapshot()) {
            if let Some(id) = save_id {
                payload["save_id"] = Value::String(id);
            }
            payload["event"] = Value::String("state".into());
            (self.emit)(payload);
        }
    }

    pub fn emit_error(&self, message: impl Into<String>) {
        (self.emit)(json!({ "event": "error", "message": message.into() }));
    }

    #[cfg(test)]
    fn save_config(&mut self, patch: impl Into<ConfigPatch>) {
        self.save_config_request(patch.into(), None, true);
    }

    fn save_config_request(&mut self, patch: ConfigPatch, id: Option<String>, resume: bool) {
        let result = self.apply_config_patch(patch);
        if let Err(err) = &result {
            self.emit_error(err);
        }
        self.emit_state(id);
        if resume && result.is_ok() && self.config.is_configured() {
            self.clear_retry();
            self.process_pending_turn();
        }
    }

    fn apply_config_patch(&mut self, patch: ConfigPatch) -> Result<(), String> {
        if let Some(capabilities) = patch.capabilities {
            let capabilities = HostConfig::normalize_capabilities(capabilities)?;
            if capabilities != self.config.capabilities {
                write_json_atomic(&self.data_dir, "capabilities.json", &capabilities)
                    .map_err(|err| format!("保存能力配置失败: {err}"))?;
            }
        }
        self.refresh_capabilities(false);

        let has_model_update = patch.providers.is_some()
            || patch.judge.is_some()
            || patch.window_max_bytes.is_some()
            || patch.max_steps.is_some();
        if has_model_update {
            let mut updated = self.config.clone();
            if let Some(providers) = patch.providers {
                updated.providers = providers;
            }
            if let Some(judge) = patch.judge {
                updated.judge = judge;
            }
            updated.window_max_bytes = patch.window_max_bytes.or(updated.window_max_bytes);
            updated.max_steps = patch.max_steps.or(updated.max_steps);
            updated.preserve_api_keys(&self.config);
            if updated != self.config {
                updated
                    .save_models(&self.core_dir)
                    .map_err(|err| format!("保存模型配置失败: {err}"))?;
                self.config = updated;
            }
        }
        Ok(())
    }

    pub fn reload_capabilities(&mut self) {
        self.refresh_capabilities(true);
        self.emit_state(None);
    }

    pub fn oauth_capability(&mut self, id: String) {
        self.refresh_capabilities(false);
        let Some(cfg) = self
            .config
            .capabilities
            .iter()
            .find(|c| c.enabled && c.id == id)
            .cloned()
        else {
            self.emit_error(format!("能力缺失或已关闭: {id}"));
            return;
        };
        let CapabilityTransport::Http { url, headers } = cfg.transport else {
            self.emit_error("仅 HTTP 能力支持 OAuth 授权");
            return;
        };
        if url.trim().is_empty() {
            self.emit_error("请先填写能力 URL");
            return;
        }
        if let Some(prev) = self.oauth_cancels.remove(&id) {
            prev.store(true, Ordering::Relaxed);
        }
        self.hands.set_oauth_code(&id, None);
        let cancel = spawn_oauth_flow(id.clone(), url, headers, self.oauth_tx.clone());
        self.oauth_cancels.insert(id, cancel);
    }

    pub fn poll_oauth_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.oauth_rx.try_recv() {
            match ev {
                OAuthEvent::DeviceCode { id, flow, code } => {
                    if self
                        .oauth_cancels
                        .get(&id)
                        .is_some_and(|current| Arc::ptr_eq(current, &flow))
                    {
                        self.hands.set_oauth_code(&id, Some(code));
                        changed = true;
                    }
                }
                OAuthEvent::Finished { id, flow, result } => {
                    if !self
                        .oauth_cancels
                        .get(&id)
                        .is_some_and(|current| Arc::ptr_eq(current, &flow))
                    {
                        continue;
                    }
                    self.oauth_cancels.remove(&id);
                    self.hands.set_oauth_code(&id, None);
                    match result {
                        Ok(token) => {
                            if let Err(err) =
                                self.hands
                                    .save_oauth_token(&id, token, &self.config.capabilities)
                            {
                                self.emit_error(format!("保存 OAuth 凭证失败: {err}"));
                            }
                        }
                        Err(err) => {
                            self.emit_error(err);
                        }
                    }
                    changed = true;
                }
            }
        }
        if changed {
            self.emit_state(None);
        }
        changed
    }

    fn test_provider(&self, id: String, mut provider: ProviderConfig, model: String) {
        provider.reuse_api_key(&self.config.providers);
        let brain = Arc::clone(&self.brain);
        let emit = Arc::clone(&self.emit);
        std::thread::spawn(move || {
            let (ok, message) = match brain.test_connection(&provider, &model) {
                Ok(message) => (true, message),
                Err(err) => (false, err),
            };
            emit(json!({ "event": "provider_test", "id": id, "ok": ok, "message": message }));
        });
    }

    fn list_provider_models(&self, id: String, mut provider: ProviderConfig) {
        provider.reuse_api_key(&self.config.providers);
        let emit = Arc::clone(&self.emit);
        std::thread::spawn(move || {
            emit(match list_models(&provider) {
                Ok(models) => json!({"event":"provider_models", "id":id, "models":models}),
                Err(err) => json!({"event":"provider_models", "id":id, "error":err}),
            });
        });
    }

    pub fn handle_user_message(&mut self, raw: &str) {
        let text = raw.trim();
        if text.is_empty() {
            return;
        }
        if let Err(err) = self.eja.notice(Source::Person, text) {
            self.emit_error(format!("写入消息失败: {err}"));
            return;
        }
        self.clear_retry();
        self.process_pending_turn();
    }

    pub fn tick_timers(&mut self) -> Result<(), String> {
        if !self.oauth_cancels.is_empty() {
            self.refresh_capabilities(false);
        }
        self.poll_oauth_events();
        let (mut fired, due_guarded, now) =
            state::collect_due_wakes(&self.eja).map_err(|err| err.to_string())?;
        let checked_watches = !due_guarded.is_empty();
        for alarm in due_guarded {
            let Some(guard) = &alarm.guard else { continue };
            let observation = state::registered_check(&self.eja, &guard.checker)
                .map_err(|err| err.to_string())
                .and_then(|check| check.matches(&self.data_dir).map_err(|err| err.to_string()));
            let retry = match &observation {
                Ok(false) if guard.remaining > 1 => now
                    .checked_add(guard.interval_seconds)
                    .map(|at| (at, format_local_unix(at))),
                _ => None,
            };
            fired |= state::settle_guarded_wake(
                &self.eja,
                &alarm,
                matches!(observation, Ok(true)),
                retry,
            )
            .map_err(|err| err.to_string())?;
            if let Err(err) = observation {
                self.emit_error(format!("守望「{}」已停止：{err}", alarm.reason));
            }
        }
        if checked_watches && !fired {
            self.emit_state(None);
        }
        if fired {
            self.clear_retry();
            self.process_pending_turn();
            return Ok(());
        }
        if self.retry_at.is_some_and(|at| Instant::now() >= at) {
            self.retry_at = None;
            self.process_pending_turn();
        }
        Ok(())
    }

    fn refresh_capabilities(&mut self, force: bool) {
        let bytes = match HostConfig::read_capabilities(&self.data_dir) {
            Ok(bytes) => bytes,
            Err(err) => return self.emit_error(err),
        };
        if !force && self.capabilities_bytes == bytes {
            if self.hands.disconnect_dead() {
                self.emit_state(None);
            }
            return;
        }
        self.capabilities_bytes = bytes;
        match HostConfig::parse_capabilities(self.capabilities_bytes.as_deref()) {
            Ok(capabilities) if force || self.config.capabilities != capabilities => {
                self.oauth_cancels.retain(|id, cancel| {
                    let previous = self.config.capabilities.iter().find(|cap| cap.id == *id);
                    let current = capabilities.iter().find(|cap| cap.id == *id && cap.enabled);
                    let keep = previous
                        .zip(current)
                        .is_some_and(|(old, new)| old.transport == new.transport);
                    if !keep {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    keep
                });
                self.hands.reload(&capabilities, force);
                self.config.capabilities = capabilities;
            }
            Ok(_) => {}
            Err(err) => self.emit_error(err),
        }
    }

    fn run_turn(&mut self, proactive: bool, trigger_id: Id) -> bool {
        self.drain_turn_commands();
        self.set_status("thinking", "思考中…");
        self.refresh_capabilities(false);
        let window_max_bytes = self.config.window_max_bytes();
        let max_steps = self.config.max_steps();
        let setup = (|| -> Result<(ProviderConfig, String, Vec<Value>), (bool, String)> {
            let Some((provider, model)) = self.config.resolve_judge() else {
                return Err((true, "请先在设置中配置可用的 LLM Provider 与主模型".into()));
            };
            let (provider, model) = (provider.clone(), model.to_string());
            if window_max_bytes < 256 {
                return Err((
                    true,
                    "上下文窗口至少需要 256 字节。请调整设置，当前请求已保留。".into(),
                ));
            }
            let head = self
                .eja
                .revision()
                .map_err(|err| (false, format!("读取日志版本失败: {err}")))?;
            let mut next_anchor = self.anchor;
            let messages = project_window_through(
                &self.eja,
                &mut next_anchor,
                head,
                window_max_bytes,
                trigger_id,
            )
            .map_err(|err| (false, format!("构建日志窗口失败: {err}")))?;
            self.persist_anchor(next_anchor)
                .map_err(|err| (false, err))?;
            Ok((provider, model, messages))
        })();
        let (provider, model, mut messages) = match setup {
            Ok(ready) => ready,
            Err((clear_retry, msg)) => {
                if clear_retry {
                    self.clear_retry();
                }
                self.emit_error(msg);
                self.set_status("idle", "在线");
                return false;
            }
        };
        let data_dir_str = self.data_dir.path().display().to_string();
        let resources_line = self
            .resources_dir
            .as_ref()
            .map(|dir| format!("\n只读协议与模板参考：{}", dir.display()))
            .unwrap_or_default();
        let local_now = self
            .eja
            .now()
            .ok()
            .map(|ts| format!("（当前本地时间：{}）", format_local_unix(ts.as_unix())))
            .unwrap_or_default();
        let system = format!(
            "你是 EJA，一个持续存在的自我同一性主体。\n\
            内核版本：{KERNEL_VERSION}（协议 v{PROTOCOL_VERSION}）{resources_line}\n\
            数据目录：{data_dir_str}\n\
            能力配置：{data_dir_str}/capabilities.json（支持 exec 本地插件、stdio 与 http MCP，变更自动热加载）\n\
            本地时区：{}{local_now}\n\n\
            前置消息是主体日志（旧日志滑出上下文窗口后折叠为活跃挂念与未决未知；跨回合约定、长期偏好与待办须通过 keep 保持，滑出窗口的历史细节用 search/read 检索）。\n\
            最后一条用户消息、定时提醒或检查命中触发本回合。不调工具时的文本回复会发送给人；定时或守望检查未满足触发条件时回复留空以保持沉默，不要播报“仍为空/未就绪”。\n\
            工具回执记录工具返回的内容。区分实测、失败、未知与推断；未经工具回执证实的外部动作不得声称已执行。若要宣告检查通过，请引用对应规则的命中记录。",
            self.local_tz
        );

        let mut completed = false;
        let mut fatal_error: Option<String> = None;
        let mut recoverable = false;
        for _ in 0..max_steps {
            self.drain_turn_commands();
            self.set_status("thinking", "思考中…");
            self.refresh_capabilities(false);
            self.poll_oauth_events();
            let tools = self.hands.tools();
            let emit = &self.emit;
            let reply = match self.brain.complete_streaming(
                &provider,
                &model,
                &system,
                &messages,
                &tools,
                &mut |delta| {
                    if !proactive && !delta.is_empty() {
                        (emit)(json!({ "event": "delta", "text": delta }));
                    }
                },
            ) {
                Ok(r) => r,
                Err(err) => {
                    recoverable = is_recoverable_llm_error(&err);
                    fatal_error = Some(err);
                    break;
                }
            };
            expire_tool_images(&mut messages, &provider.kind);

            if reply.calls.is_empty() {
                let text = normalize_reply_text(&reply.text, proactive);
                if let Err(err) = state::complete_turn(&self.eja, trigger_id, text) {
                    fatal_error = Some(format!("保存回合结算状态失败: {err}"));
                    break;
                }
                if proactive && !text.is_empty() {
                    (self.emit)(json!({ "event": "notify", "title": "EJA", "body": text }));
                }
                completed = true;
                break;
            }

            let step_journal_head = match self.eja.revision() {
                Ok(rev) => rev,
                Err(err) => {
                    fatal_error = Some(err.to_string());
                    break;
                }
            };
            let step = tool_messages(&provider.kind, reply, window_max_bytes, |call| {
                self.drain_turn_commands();
                self.refresh_capabilities(false);
                self.set_status("thinking", activity_label(&call.name));
                self.execute_tool_call(call, &tools)
            });
            let mut anchor = self.anchor;
            let applied = step.and_then(|step| {
                apply_tool_exchange(
                    step,
                    &mut messages,
                    &self.eja,
                    &mut anchor,
                    step_journal_head,
                    trigger_id,
                    window_max_bytes,
                )
                .map_err(|err| format!("重投影日志窗口失败: {err}"))?;
                self.persist_anchor(anchor)
            });
            if let Err(err) = applied {
                fatal_error = Some(err);
                break;
            }
        }

        if completed {
            self.clear_retry();
        } else if let Some(err) = fatal_error {
            if recoverable && self.retry_attempts < MAX_RETRY_ATTEMPTS {
                let delay_secs = 5u64 * 3u64.pow(u32::from(self.retry_attempts));
                self.retry_attempts += 1;
                self.retry_at = Some(Instant::now() + Duration::from_secs(delay_secs));
            } else {
                self.clear_retry();
                completed = state::complete_turn(&self.eja, trigger_id, "").is_ok();
            }
            self.emit_error(format!("执行回合失败: {err}"));
        } else {
            self.clear_retry();
            completed = state::complete_turn(&self.eja, trigger_id, "").is_ok();
            self.emit_error(format!("达到单回合最大步数上限 ({max_steps})，回合已暂停"));
        }
        self.set_status("idle", "在线");
        completed
    }

    fn execute_tool_call(
        &mut self,
        call: &ToolCall,
        tools: &[ToolSpec],
    ) -> Result<ToolOutput, String> {
        let result = match call.name.as_str() {
            "keep" => {
                let text = call.args["text"].as_str().unwrap_or("").trim();
                let checker = match call.args.get("checker") {
                    None | Some(Value::Null) => None,
                    Some(value) => match value.as_str() {
                        Some(name) => Some(name),
                        None => {
                            return Ok(ToolOutput::Value(json!({"error":"checker 需要规则名称"})))
                        }
                    },
                };
                state::keep_with_check(&self.eja, call.args["concern"].as_i64(), text, checker).map(
                    |entry| {
                        let Fact::Kept { concern, .. } = entry.fact else {
                            unreachable!()
                        };
                        ToolOutput::Value(
                            json!({"status":"kept", "concern":concern, "fact_id":entry.id}),
                        )
                    },
                )
            }
            "release" => match call.args["concern"].as_i64() {
                Some(id) => {
                    let witness = call.args["witness_fact_id"].as_i64();
                    state::release_with_witness(&self.eja, id, witness).map(|entry| {
                        ToolOutput::Value(json!({
                            "status": "released",
                            "concern": id,
                            "fact_id": entry.id,
                            "witness_fact_id": witness
                        }))
                    })
                }
                None => Err(eja_core::Error::Invalid("release 需要整数 concern".into())),
            },
            "read" => match call.args["fact"].as_i64() {
                Some(id) => {
                    let offset = call.args["offset"].as_u64().unwrap_or(0) as usize;
                    self.eja.evidence(id).map(|ev| ToolOutput::Read(ev, offset))
                }
                None => Err(eja_core::Error::Invalid("read 需要整数 fact".into())),
            },
            "search" => {
                let cue = call.args["cue"].as_str().unwrap_or("").trim();
                return match self.memory.search(&self.eja, cue) {
                    Ok(matches) => Ok(ToolOutput::Value(json!({"cue":cue,"matches":matches}))),
                    Err(eja_memory::Error::Invalid(msg)) => {
                        Ok(ToolOutput::Value(json!({"error": msg})))
                    }
                    Err(err) => Err(err.to_string()),
                };
            }
            other => {
                let Some(spec) = tools
                    .iter()
                    .find(|t| t.target != ToolTarget::Kernel && t.name == other)
                else {
                    return Ok(ToolOutput::Value(
                        json!({ "error": format!("未知工具: {other}") }),
                    ));
                };
                if !self.hands.is_current(spec) {
                    return Ok(ToolOutput::Value(
                        json!({"error":"能力已关闭或变更，请按当前工具列表重试"}),
                    ));
                }
                let revision = self.eja.revision().map_err(|err| err.to_string())?;
                let acted = self.eja.act(revision, &spec.capability, call.args.clone());
                let acted = match acted {
                    Ok(c) => c,
                    Err(eja_core::Error::Invalid(msg)) => {
                        return Ok(ToolOutput::Value(json!({ "error": msg })))
                    }
                    Err(err) => return Err(err.to_string()),
                };
                let execution = self.hands.execute(&acted, &self.eja);
                let receipt = self
                    .eja
                    .finish(acted.id, execution.outcome)
                    .map_err(|err| err.to_string())?;
                if self.hands.disconnect_dead() {
                    self.emit_state(None);
                }
                let Fact::Returned { outcome, .. } = receipt.fact else {
                    unreachable!()
                };
                let value = json!({ "id": receipt.id, "action": acted.id, "outcome": outcome });
                Ok(if execution.images.is_empty() {
                    ToolOutput::Value(value)
                } else {
                    ToolOutput::Images(value, execution.images)
                })
            }
        };
        match result {
            Ok(output) => Ok(output),
            Err(eja_core::Error::Invalid(msg)) => Ok(ToolOutput::Value(json!({"error":msg}))),
            Err(err) => Err(err.to_string()),
        }
    }

    fn set_status(&mut self, status: &str, detail: &str) {
        if self.status.0 != status || self.status.1 != detail {
            let phase_changed = self.status.0 != status;
            self.status = (status.to_string(), detail.to_string());
            if phase_changed {
                self.emit_state(None);
            } else {
                (self.emit)(json!({ "event": "activity", "status_detail": detail }));
            }
        }
    }
}

impl Drop for HostRuntime {
    fn drop(&mut self) {
        let _ = self._instance_lock.unlock();
    }
}

fn open_memory(index_path: &Path) -> Result<MemoryIndex, String> {
    #[cfg(feature = "onnx")]
    if let Ok(model_dir) = std::env::var("EJA_ONNX_MODEL_DIR") {
        if Path::new(&model_dir).exists() {
            if let Ok(enc) = eja_memory::OnnxEncoder::load(&model_dir) {
                return MemoryIndex::open(index_path, enc).map_err(|err| err.to_string());
            }
        }
    }
    let _ = index_path;
    Ok(MemoryIndex::keyword())
}

fn build_stream(entries: &[Entry], proactive_replies: &HashSet<Id>) -> Vec<StreamItem> {
    let mut returns: HashMap<Id, &Outcome> = HashMap::new();
    for entry in entries {
        if let Fact::Returned { action, outcome } = &entry.fact {
            returns.insert(*action, outcome);
        }
    }
    let mut items = Vec::new();
    for entry in entries {
        match &entry.fact {
            Fact::Noticed { source, text } => {
                let kind = if *source == Source::Person {
                    "person"
                } else {
                    "event"
                };
                items.push(StreamItem {
                    id: entry.id,
                    kind: kind.into(),
                    text: text.clone(),
                    detail: None,
                    proactive: None,
                    outcome_status: None,
                    truncated: None,
                });
            }
            Fact::Said { text } => {
                let proactive = proactive_replies.contains(&entry.id);
                items.push(StreamItem {
                    id: entry.id,
                    kind: "eja".into(),
                    text: text.clone(),
                    detail: None,
                    proactive: proactive.then_some(true),
                    outcome_status: None,
                    truncated: None,
                });
            }
            Fact::Acted { capability, input } => {
                let outcome = returns.get(&entry.id).copied();
                let (outcome_text, outcome_status, truncated) = match outcome {
                    Some(Outcome::Observed(s)) => (
                        Some(s.clone()),
                        Some("observed".to_string()),
                        is_truncated_receipt(s).then_some(true),
                    ),
                    Some(Outcome::Failed(s)) => (
                        Some(format!("失败: {s}")),
                        Some("failed".to_string()),
                        is_truncated_receipt(s).then_some(true),
                    ),
                    Some(Outcome::Unknown(s)) => (
                        Some(format!("未知: {s}")),
                        Some("unknown".to_string()),
                        is_truncated_receipt(s).then_some(true),
                    ),
                    None => (None, Some("pending".to_string()), None),
                };
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
                    outcome_status,
                    truncated,
                });
            }
            _ => {}
        }
    }
    items
}

fn is_truncated_receipt(detail: &str) -> bool {
    detail.contains("… [truncated;") || detail.contains("[output truncated at ")
}

fn normalize_reply_text(raw: &str, proactive: bool) -> &str {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "";
    }
    if !proactive {
        return trimmed;
    }
    let stripped = trimmed
        .trim_matches(|ch: char| matches!(ch, '(' | ')' | '（' | '）' | '[' | ']' | '【' | '】'))
        .trim();
    if stripped.is_empty()
        || matches!(
            stripped,
            "保持沉默" | "沉默" | "静默" | "无需回复" | "不打扰"
        )
        || stripped.eq_ignore_ascii_case("silent")
        || stripped.eq_ignore_ascii_case("silence")
        || stripped.eq_ignore_ascii_case("no_reply")
    {
        ""
    } else {
        trimmed
    }
}

fn is_recoverable_llm_error(err: &str) -> bool {
    if let Some(rest) = err.strip_prefix("HTTP ") {
        let code = rest
            .split(|ch: char| !ch.is_ascii_digit())
            .next()
            .and_then(|digits| digits.parse::<u16>().ok())
            .unwrap_or(0);
        return matches!(code, 408 | 429 | 500..=599);
    }
    err.starts_with("网络请求失败") || err.starts_with("等待 LLM 响应失败")
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
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let resources = macos_dir.parent()?.join("Resources");
    resources.is_dir().then_some(resources)
}

enum LockError {
    AlreadyRunning(Option<u32>),
    Io(String),
}

fn lock_core_dir(core_dir: &Path) -> Result<fs::File, LockError> {
    fs::create_dir_all(core_dir)
        .map_err(|err| LockError::Io(format!("准备内核目录失败: {err}")))?;
    let lock_path = core_dir.join("host.lock");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| LockError::Io(format!("打开实例锁失败: {err}")))?;
    match file.try_lock() {
        Ok(()) => {
            let _ = file.set_len(0);
            let _ = file.seek(SeekFrom::Start(0));
            #[cfg(unix)]
            let owner_pid = std::os::unix::process::parent_id();
            #[cfg(not(unix))]
            let owner_pid = std::process::id();
            let _ = writeln!(file, "{owner_pid}");
            let _ = file.flush();
            Ok(file)
        }
        Err(fs::TryLockError::WouldBlock) => {
            let ui_pid = fs::read_to_string(&lock_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            Err(LockError::AlreadyRunning(ui_pid))
        }
        Err(fs::TryLockError::Error(err)) => Err(LockError::Io(format!("获取实例锁失败: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityConfig, CapabilityTransport, ModelRef, ProviderKind};
    use crate::llm::{messages_bytes, ModelReply};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

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
                        .find(|t| t.capability.starts_with("cap."))
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

    fn configured(capabilities: Vec<CapabilityConfig>) -> HostConfig {
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
            capabilities: HostConfig::normalize_capabilities(capabilities).unwrap(),
            window_max_bytes: None,
            max_steps: None,
        }
    }

    #[test]
    fn user_turn_records_stream_and_runs_multiple_turns() {
        let dir = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let events_sink = events.clone();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain.clone(),
            Arc::new(move |ev| events_sink.lock().unwrap().push(ev)),
        )
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
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|ev| ev["event"] == "delta" && ev["text"] == "你好，我在。"));
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);

        rt.handle_user_message("第 2 轮");
        rt.handle_user_message("第 3 轮");
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn unified_capabilities_builtin_exec_and_stdio_discover_and_invoke_in_turn() {
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
        let exec_script = dir.path().join("plugin_exec.py");
        fs::write(
            &exec_script,
            "import json, sys\nargs = json.load(sys.stdin)\nprint('exec:' + args.get('msg', ''))\n",
        )
        .unwrap();

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "mcp",
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain,
            Arc::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
        rt.save_config(configured(vec![
            CapabilityConfig {
                id: "demo".into(),
                name: "Demo MCP".into(),
                enabled: true,
                transport: CapabilityTransport::Stdio {
                    command: "/usr/bin/python3".into(),
                    args: vec![script_path.display().to_string()],
                    env: BTreeMap::new(),
                },
            },
            CapabilityConfig {
                id: "plug".into(),
                name: "Local Plugin".into(),
                enabled: true,
                transport: CapabilityTransport::Exec {
                    command: "/usr/bin/python3".into(),
                    args: vec![exec_script.display().to_string()],
                    env: BTreeMap::new(),
                    description: "Local free-form exec capability".into(),
                    schema: None,
                },
            },
        ]));

        let snap = rt.snapshot();
        assert_eq!(snap.capability_statuses.len(), 3);
        assert_eq!(snap.capability_statuses[0].id, "shell");
        assert!(snap.capability_statuses[0].connected);
        assert_eq!(snap.capability_statuses[0].tools[0].capability, "shell");
        assert!(snap.capability_statuses[1].connected);
        assert_eq!(
            snap.capability_statuses[1].tools[0].capability,
            "cap.demo.echo"
        );
        assert!(snap.capability_statuses[2].connected);
        assert_eq!(snap.capability_statuses[2].tools[0].capability, "cap.plug");
        let exec_call = eja_core::Call {
            id: 1,
            capability: "cap.plug".into(),
            input: json!({ "msg": "free" }),
        };
        assert_eq!(
            rt.hands.execute(&exec_call, &rt.eja).outcome,
            Outcome::Observed("exec:free".into())
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
        let events = events.lock().unwrap();
        let activity: Vec<_> = events
            .iter()
            .filter_map(|e| e["status_detail"].as_str())
            .collect();
        assert!(activity.ends_with(&["思考中…", "cap_demo_echo", "思考中…", "在线"]));
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
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
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

        // Exceed the window budget so the persisted anchor advances.
        rt.handle_user_message(&"长消息".repeat(120));
        rt.handle_user_message("推进窗口");
        drop(rt);

        // Reopen runtime and verify persisted anchor starts cleanly with the open concern snapshot.
        let mut restarted =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
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
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
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
    fn split_config_and_capabilities_and_preserve_state_on_invalid_capabilities_json() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));

        let core_config = HostConfig::config_path(&dir.path().join("ai.eja.kernel"));
        let capabilities_path = HostConfig::capabilities_path(&dir.path().join("Eja"));
        assert!(core_config.exists());
        assert!(!capabilities_path.exists());
        assert!(!dir.path().join("Eja/core").exists());
        assert!(!fs::read_to_string(&core_config)
            .unwrap()
            .contains("capabilities"));

        fs::write(&capabilities_path, "{ broken json").unwrap();
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
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Arc::new(|_| {})).unwrap();
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
        let disk =
            fs::read_to_string(HostConfig::config_path(&dir.path().join("ai.eja.kernel"))).unwrap();
        assert!(disk.contains("sk-top-secret"));
        assert!(!disk.contains("has_api_key"));

        let mut edited = rt.snapshot().config;
        edited.providers[0].base_url = "http://127.0.0.1:10000/v1".into();
        rt.save_config(edited);
        assert!(!rt.snapshot().config.providers[0].has_api_key);
        let disk =
            fs::read_to_string(HostConfig::config_path(&dir.path().join("ai.eja.kernel"))).unwrap();
        assert!(!disk.contains("sk-top-secret"));
    }

    #[test]
    fn capability_change_during_a_model_step_revokes_the_advertised_tool() {
        struct SwitchingBrain(Mutex<Option<Sender<Result<HostCommand, String>>>>);
        impl Brain for SwitchingBrain {
            fn complete(
                &self,
                _: &ProviderConfig,
                _: &str,
                _: &str,
                messages: &[Value],
                tools: &[ToolSpec],
            ) -> Result<ModelReply, String> {
                if let Some(send) = self.0.lock().unwrap().take() {
                    assert!(tools.iter().any(|tool| tool.name == "cap_toggle"));
                    let command = serde_json::from_value(json!({
                        "cmd":"save_config", "id":"switch",
                        "config":{"capabilities":[{"id":"toggle","name":"Toggle","enabled":false,
                            "transport":{"type":"exec","command":"/bin/cat"}}]}
                    }))
                    .unwrap();
                    send.send(Ok(command)).unwrap();
                    return Ok(tool_reply("a1", "cap_toggle", json!({})));
                }
                assert!(!tools.iter().any(|tool| tool.name == "cap_toggle"));
                assert!(messages.last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("能力已关闭"));
                Ok(text_reply("已关闭"))
            }

            fn test_connection(&self, _: &ProviderConfig, _: &str) -> Result<String, String> {
                unreachable!()
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let brain = Arc::new(SwitchingBrain(Mutex::new(Some(tx))));
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&events);
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain,
            Arc::new(move |event| received.lock().unwrap().push(event)),
        )
        .unwrap();
        let enabled = serde_json::from_value(json!({"id":"toggle","name":"Toggle",
            "transport":{"type":"exec","command":"/bin/cat"}}))
        .unwrap();
        rt.save_config(configured(vec![enabled]));
        rt.inbox = Some(rx);
        rt.handle_user_message("检查开关");
        assert!(!rt
            .eja
            .recent(10)
            .unwrap()
            .iter()
            .any(|entry| matches!(entry.fact, Fact::Acted { .. })));
        assert!(!rt.snapshot().config.capabilities[1].enabled);
        assert!(events.lock().unwrap().iter().any(|event| {
            event["event"] == "state"
                && event["save_id"] == "switch"
                && event["config"]["capabilities"][1]["enabled"] == false
        }));
    }

    #[test]
    fn queued_oauth_events_cannot_settle_a_replacement_flow() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = HostRuntime::open(dir.path().join("Eja"), Arc::new(|_| {})).unwrap();
        let old = Arc::new(AtomicBool::new(true));
        let current = Arc::new(AtomicBool::new(false));
        rt.oauth_cancels.insert("http".into(), Arc::clone(&current));
        rt.oauth_tx
            .send(OAuthEvent::DeviceCode {
                id: "http".into(),
                flow: Arc::clone(&old),
                code: "OLD".into(),
            })
            .unwrap();
        rt.oauth_tx
            .send(OAuthEvent::Finished {
                id: "http".into(),
                flow: old,
                result: Ok(crate::oauth::OAuthToken::default()),
            })
            .unwrap();
        assert!(!rt.poll_oauth_events());
        assert!(Arc::ptr_eq(&rt.oauth_cancels["http"], &current));
        assert!(!dir.path().join("ai.eja.kernel/oauth.json").exists());
    }

    #[test]
    fn detects_kernel_upgrade_and_emits_world_notice_once() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        assert!(rt.snapshot().stream.is_empty());
        state::set_kernel_version_for_test(&rt.eja, "0.9.0").unwrap();
        drop(rt);

        let upgraded =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Arc::new(|_| {})).unwrap();
        assert!(upgraded
            .snapshot()
            .stream
            .iter()
            .any(|item| item.text.contains("0.9.0") && item.text.contains(KERNEL_VERSION)));
    }

    #[test]
    fn rejects_second_instance_on_same_data_dir_until_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt1 =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let second = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain.clone(),
            Arc::new(move |val| sink_events.lock().unwrap().push(val)),
        );
        assert!(matches!(second, Err(OpenError::AlreadyRunning)));
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["event"] == "already_running"));

        drop(rt1);
        assert!(
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Arc::new(|_| {}),).is_ok()
        );
    }

    #[test]
    fn restart_reconciles_unfinished_actions_once() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt = HostRuntime::open_with_brain(data_dir.clone(), brain.clone(), Arc::new(|_| {}))
            .unwrap();
        let dangling = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "shell",
                json!({ "command": "sleep 999" }),
            )
            .unwrap();
        drop(rt);

        let rt = HostRuntime::open_with_brain(data_dir.clone(), brain.clone(), Arc::new(|_| {}))
            .unwrap();
        let snap = rt.snapshot();
        assert!(snap.stream.iter().any(|item| {
            item.id == dangling.id
                && item
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("未知:") && d.contains("中断"))
        }));
        let rev_after_first = rt.eja.revision().unwrap();
        drop(rt);

        let reopened = HostRuntime::open_with_brain(data_dir, brain, Arc::new(|_| {})).unwrap();
        assert_eq!(reopened.eja.revision().unwrap(), rev_after_first);
    }

    struct ScriptedBrain {
        replies: Mutex<Vec<Result<ModelReply, String>>>,
        seen_windows: Mutex<Vec<Vec<Value>>>,
    }

    impl Brain for ScriptedBrain {
        fn complete(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            _system: &str,
            messages: &[Value],
            _tools: &[ToolSpec],
        ) -> Result<ModelReply, String> {
            self.seen_windows.lock().unwrap().push(messages.to_vec());
            self.replies.lock().unwrap().remove(0)
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
    fn due_wake_persists_pending_across_unconfigured_restart_and_resumes_on_config() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(HostConfig {
            providers: Vec::new(),
            judge: ModelRef::default(),
            capabilities: Vec::new(),
            window_max_bytes: None,
            max_steps: None,
        });
        state::schedule_wakes(
            &rt.eja,
            &[WakeAlarm {
                id: "w-1".into(),
                fire_at_unix: 1,
                fire_at_display: "00:00:01".into(),
                reason: "喝水提醒".into(),
                guard: None,
            }],
        )
        .unwrap();
        rt.tick_timers().unwrap();
        assert!(rt.has_pending_trigger());
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 0);
        drop(rt);

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let mut restarted = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain.clone(),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
        assert!(restarted.has_pending_trigger());
        restarted.save_config(configured(vec![]));
        assert!(!restarted.has_pending_trigger());
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "notify" && event["body"] == "你好，我在。"));
        drop(restarted);

        let mut settled =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        assert!(!settled.has_pending_trigger());
        settled.process_pending_turn();
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn intentional_silence_settles_and_recoverable_failure_retries_without_replaying_actions() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                Ok(text_reply("   ")),
                Err("HTTP 503: temporary".into()),
                Err("HTTP 503: still down".into()),
                Ok(text_reply("恢复完成")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));
        rt.handle_user_message("保持沉默测试");
        assert!(!rt.has_pending_trigger());
        assert!(rt.retry_at.is_none());

        rt.handle_user_message("故障恢复测试");
        assert!(rt.has_pending_trigger());
        assert!(rt.retry_at.is_some());
        assert!(rt.next_timer_delay().unwrap().is_some());
        rt.retry_at = Some(Instant::now());
        rt.tick_timers().unwrap();
        assert!(rt.has_pending_trigger());
        assert_eq!(rt.retry_attempts, 2);

        let rev = rt.eja.revision().unwrap();
        let acted = rt
            .eja
            .act(rev, "shell", json!({ "command": "echo once" }))
            .unwrap();
        drop(rt);

        let mut restarted =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        assert!(restarted.has_pending_trigger());
        restarted.process_pending_turn();
        assert!(!restarted.has_pending_trigger());

        let windows = brain.seen_windows.lock().unwrap();
        let resumed_window = windows.last().unwrap();
        assert!(resumed_window.iter().any(|msg| {
            msg["content"].as_str().is_some_and(|text| {
                text.contains(&format!("\"action\":{}", acted.id)) && text.contains("unknown")
            })
        }));
        let snap = restarted.snapshot();
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "恢复完成"));
    }

    #[test]
    fn max_steps_and_unrecoverable_failures_settle_trigger_without_blocking_later_turns() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                Ok(tool_reply("s1", "search", json!({ "cue": "咖啡" }))),
                Ok(tool_reply("s2", "search", json!({ "cue": "老地方" }))),
                Ok(text_reply("G1 之后正常回复")),
                Err("HTTP 400: invalid_request_error".into()),
                Ok(text_reply("400 之后正常回复")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&events);
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain.clone(),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
        let mut cfg = configured(vec![]);
        cfg.max_steps = Some(2);
        rt.save_config(cfg);

        rt.handle_user_message("G1 连续检索触发步数上限");
        assert!(!rt.has_pending_trigger());
        assert!(rt.retry_at.is_none());
        assert!(events.lock().unwrap().iter().any(|ev| {
            ev["event"] == "error"
                && ev["message"]
                    .as_str()
                    .is_some_and(|msg| msg.contains("达到单回合最大步数上限 (2)"))
        }));

        rt.handle_user_message("E1 我叫什么名字？");
        assert!(!rt.has_pending_trigger());

        rt.handle_user_message("触发不可恢复 400");
        assert!(!rt.has_pending_trigger());
        assert!(rt.retry_at.is_none());

        rt.handle_user_message("后续消息仍可执行");
        assert!(!rt.has_pending_trigger());

        let snap = rt.snapshot();
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "G1 之后正常回复"));
        assert!(snap
            .stream
            .iter()
            .any(|item| item.kind == "eja" && item.text == "400 之后正常回复"));
    }

    #[test]
    fn partial_save_config_preserves_other_domain() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Arc::new(|_| {})).unwrap();
        let mut cfg = configured(vec![]);
        cfg.providers[0].api_key = "sk-keep-me".into();
        cfg.max_steps = Some(7);
        cfg.window_max_bytes = Some(4096);
        rt.save_config(cfg);

        let external_capabilities = vec![CapabilityConfig {
            id: "ext".into(),
            name: "External".into(),
            enabled: false,
            transport: CapabilityTransport::Http {
                url: "http://127.0.0.1:8080/mcp".into(),
                headers: BTreeMap::new(),
            },
        }];
        let data_root = TrustedDir::open(dir.path().join("Eja")).unwrap();
        write_json_atomic(&data_root, "capabilities.json", &external_capabilities).unwrap();
        rt.reload_capabilities();

        let mut renamed = rt.snapshot().config.providers;
        renamed[0].name = "Renamed Local".into();
        rt.save_config(ConfigPatch {
            providers: Some(renamed),
            judge: Some(rt.snapshot().config.judge),
            ..ConfigPatch::default()
        });

        let expected_capabilities =
            HostConfig::normalize_capabilities(external_capabilities.clone()).unwrap();
        assert_eq!(rt.snapshot().config.capabilities, expected_capabilities);
        assert_eq!(rt.snapshot().config.providers[0].name, "Renamed Local");
        assert_eq!(rt.config.max_steps, Some(7));
        assert_eq!(rt.config.window_max_bytes, Some(4096));
        let disk_capabilities: Vec<CapabilityConfig> = serde_json::from_slice(
            &fs::read(HostConfig::capabilities_path(&dir.path().join("Eja"))).unwrap(),
        )
        .unwrap();
        assert_eq!(disk_capabilities, external_capabilities);

        rt.save_config(ConfigPatch {
            capabilities: Some(Vec::new()),
            ..ConfigPatch::default()
        });
        assert_eq!(
            rt.snapshot().config.capabilities,
            vec![CapabilityConfig::builtin_shell()]
        );
        assert!(rt.snapshot().config.providers[0].has_api_key);
    }

    struct MultiSearchBrain {
        max_seen_bytes: Mutex<usize>,
        step: AtomicUsize,
    }

    impl Brain for MultiSearchBrain {
        fn complete(
            &self,
            _provider: &ProviderConfig,
            _model: &str,
            _system: &str,
            messages: &[Value],
            _tools: &[ToolSpec],
        ) -> Result<ModelReply, String> {
            let bytes = messages_bytes(messages);
            let mut guard = self.max_seen_bytes.lock().unwrap();
            *guard = (*guard).max(bytes);
            if self.step.fetch_add(1, Ordering::SeqCst) < 3 {
                return Ok(tool_reply(
                    &format!("s{}", self.step.load(Ordering::SeqCst)),
                    "search",
                    json!({ "cue": "alpha" }),
                ));
            }
            Ok(text_reply("done"))
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
    fn in_turn_tool_results_stay_within_window_budget() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MultiSearchBrain {
            max_seen_bytes: Mutex::new(0),
            step: AtomicUsize::new(0),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        let mut cfg = configured(vec![]);
        cfg.window_max_bytes = Some(4096);
        rt.save_config(cfg);
        let blob = "alpha ".repeat(1200);
        for _ in 0..4 {
            rt.eja.notice(Source::World, &blob).unwrap();
        }

        rt.handle_user_message("检索 alpha");
        assert!(*brain.max_seen_bytes.lock().unwrap() <= 4096);
    }

    struct CompactionAuditBrain {
        seen_windows: Mutex<Vec<Vec<Value>>>,
        mode: &'static str,
        step: AtomicUsize,
    }

    impl Brain for CompactionAuditBrain {
        fn complete(
            &self,
            provider: &ProviderConfig,
            _model: &str,
            _system: &str,
            messages: &[Value],
            _tools: &[ToolSpec],
        ) -> Result<ModelReply, String> {
            self.seen_windows.lock().unwrap().push(messages.to_vec());
            let current = self.step.fetch_add(1, Ordering::SeqCst);
            let read_reply = |id: String, args: Value| {
                let mut reply = tool_reply(&id, "read", args.clone());
                if provider.kind == ProviderKind::Anthropic {
                    reply.message = json!({"role":"assistant", "content":[{"type":"tool_use","id":id,"name":"read","input":args}]});
                }
                reply
            };
            match (self.mode, current) {
                ("preserve_trigger", 0) => Ok(tool_reply(
                    "c1",
                    "shell",
                    json!({ "command": "python3 -c 'print(\"z\" * 2500)'" }),
                )),
                ("oversized_call", 0) => Ok(tool_reply(
                    "c1",
                    "shell",
                    json!({ "command": format!("echo {}", "a".repeat(3000)) }),
                )),
                ("read_large_fact", 0) => Ok(tool_reply("r1", "read", json!({ "fact": 1 }))),
                ("read_paginated", 0) => Ok(read_reply("r1".into(), json!({"fact": 1}))),
                ("read_paginated", _) => {
                    let last = messages.last().unwrap();
                    let body: Value = serde_json::from_str(
                        last["content"]
                            .as_str()
                            .or_else(|| last["content"][0]["content"].as_str())
                            .unwrap(),
                    )
                    .unwrap();
                    if body["truncated"] == true {
                        let next = body["next_offset"].as_u64().unwrap();
                        return Ok(read_reply(
                            format!("r{}", current + 1),
                            json!({ "fact": 1, "offset": next }),
                        ));
                    }
                    Ok(text_reply("read complete"))
                }
                _ => Ok(text_reply("finished")),
            }
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
    fn mid_turn_compaction_preserves_unfinished_request_and_bounds_oversized_calls() {
        for mode in ["preserve_trigger", "oversized_call"] {
            let dir = tempfile::tempdir().unwrap();
            let brain = Arc::new(CompactionAuditBrain {
                seen_windows: Mutex::new(Vec::new()),
                mode,
                step: AtomicUsize::new(0),
            });
            let mut rt = HostRuntime::open_with_brain(
                dir.path().join("Eja"),
                brain.clone(),
                Arc::new(|_| {}),
            )
            .unwrap();
            let mut cfg = configured(vec![]);
            cfg.window_max_bytes = Some(2048);
            rt.save_config(cfg);
            for i in 0..4 {
                rt.eja
                    .notice(Source::World, format!("old-{i}-{}", "q".repeat(400)))
                    .unwrap();
            }

            let marker = "CRITICAL_UNFINISHED_USER_REQUEST";
            rt.handle_user_message(marker);

            let windows = brain.seen_windows.lock().unwrap();
            assert_eq!(windows.len(), 2, "mode {mode} should run 2 steps");
            for (idx, win) in windows.iter().enumerate() {
                assert!(
                    messages_bytes(win) <= 2048,
                    "mode {mode} step {idx} exceeded budget: {}",
                    messages_bytes(win)
                );
                assert!(
                    win.iter().any(|msg| msg["content"]
                        .as_str()
                        .is_some_and(|text| text.contains(marker))),
                    "mode {mode} step {idx} dropped user trigger"
                );
            }
        }
    }

    #[test]
    fn read_tool_returns_full_60k_fact_in_default_window_and_paginates_in_small_window() {
        let big = "记".repeat(20_000);

        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(CompactionAuditBrain {
            seen_windows: Mutex::new(Vec::new()),
            mode: "read_large_fact",
            step: AtomicUsize::new(0),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));
        let fact_id = rt.eja.notice(Source::World, &big).unwrap();
        assert_eq!(fact_id, 1);
        rt.handle_user_message("读第 1 条");
        let windows = brain.seen_windows.lock().unwrap();
        let tool_msg = windows[1].last().unwrap();
        let payload: Value = serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["id"], 1);
        assert_eq!(payload["fact"]["text"], big);

        let escaped = "{\"id\":12,\"active\":true}\n".repeat(1000);
        for (kind, body) in [
            (ProviderKind::OpenAi, &big),
            (ProviderKind::Anthropic, &escaped),
        ] {
            let small_dir = tempfile::tempdir().unwrap();
            let small_brain = Arc::new(CompactionAuditBrain {
                seen_windows: Mutex::new(Vec::new()),
                mode: "read_paginated",
                step: AtomicUsize::new(0),
            });
            let mut small_rt = HostRuntime::open_with_brain(
                small_dir.path().join("Eja"),
                small_brain.clone(),
                Arc::new(|_| {}),
            )
            .unwrap();
            let mut cfg = configured(vec![]);
            cfg.window_max_bytes = Some(8192);
            cfg.providers[0].kind = kind;
            small_rt.save_config(cfg);
            let id = small_rt.eja.notice(Source::World, body).unwrap();
            let original = serde_json::to_string(&small_rt.eja.evidence(id).unwrap()).unwrap();
            small_rt.handle_user_message("分页读第 1 条");

            let small_windows = small_brain.seen_windows.lock().unwrap();
            assert!(small_windows.len() > 2);
            for win in small_windows.iter() {
                assert!(messages_bytes(win) <= 8192);
            }
            let mut reassembled = String::new();
            for window in &small_windows[1..] {
                let content = &window.last().unwrap()["content"];
                let page: Value = serde_json::from_str(
                    content
                        .as_str()
                        .or_else(|| content[0]["content"].as_str())
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(page["offset"], reassembled.len());
                reassembled.push_str(page["chunk"].as_str().unwrap());
                if page["truncated"] == true {
                    assert_eq!(page["next_offset"], reassembled.len());
                }
            }
            assert_eq!(reassembled, original);
        }
    }

    #[test]
    fn silent_turn_persists_the_wake_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Eja");
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                Ok(tool_reply(
                    "w",
                    "wake",
                    json!({"delay_seconds": 3600, "reason": "once"}),
                )),
                Ok(text_reply("")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(data.clone(), brain.clone(), Arc::new(|_| {})).unwrap();
        rt.save_config(configured(vec![]));
        rt.handle_user_message("请预约一次，随后保持沉默");
        assert!(!rt.has_pending_trigger());
        assert_eq!(state::pending_wakes(&rt.eja).unwrap().len(), 1);
        drop(rt);
        let mut rt = HostRuntime::open_with_brain(data, brain.clone(), Arc::new(|_| {})).unwrap();
        rt.process_pending_turn();
        assert_eq!(state::pending_wakes(&rt.eja).unwrap().len(), 1);
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 2);
    }

    #[test]
    fn unusable_window_keeps_the_request_until_configuration_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![Ok(text_reply("seen"))]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        let mut cfg = configured(vec![]);
        cfg.window_max_bytes = Some(1);
        rt.save_config(cfg);
        rt.handle_user_message("pending");
        assert!(rt.has_pending_trigger());
        assert!(brain.seen_windows.lock().unwrap().is_empty());
        let mut cfg = configured(vec![]);
        cfg.window_max_bytes = Some(256);
        rt.save_config(cfg);
        assert!(!rt.has_pending_trigger());
        let windows = brain.seen_windows.lock().unwrap();
        assert_eq!(windows.len(), 1);
        assert!(messages_bytes(&windows[0]) <= 256);
    }

    #[test]
    fn simultaneous_wakes_each_reach_the_model_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![Ok(text_reply("first")), Ok(text_reply("second"))]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        let mut cfg = configured(vec![]);
        cfg.window_max_bytes = Some(2048);
        rt.save_config(cfg);
        let alarms = ["alarm-alpha", "alarm-beta"].map(|marker| WakeAlarm {
            id: marker.into(),
            fire_at_unix: 1,
            fire_at_display: "00:00:01".into(),
            reason: format!("{marker} {}", "x".repeat(1400)),
            guard: None,
        });
        state::schedule_wakes(&rt.eja, &alarms).unwrap();
        rt.tick_timers().unwrap();
        let windows = brain.seen_windows.lock().unwrap();
        assert_eq!(windows.len(), 2);
        let contains = |window: &[Value], marker: &str| {
            window
                .iter()
                .any(|m| m["content"].as_str().is_some_and(|s| s.contains(marker)))
        };
        assert!(contains(&windows[0], "alarm-alpha"));
        assert!(!contains(&windows[0], "alarm-beta"));
        assert!(contains(&windows[1], "alarm-beta"));
        assert_eq!(windows[1].last().unwrap()["role"], "user");
        assert!(windows.iter().all(|w| messages_bytes(w) <= 2048));
        assert!(!rt.has_pending_trigger());
    }

    #[test]
    fn saved_anchor_at_pending_trigger_restores_user_request() {
        let dir = tempfile::tempdir().unwrap();
        let core = dir.path().join("ai.eja.kernel");
        fs::create_dir(&core).unwrap();
        let eja = Eja::open(core.join("subject.db")).unwrap();
        eja.notice(Source::Person, "original request").unwrap();
        eja.keep(None, "later concern").unwrap();
        state::save_anchor(&eja, 1).unwrap();
        drop(eja);
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![Ok(text_reply(""))]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Arc::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));
        assert!(brain.seen_windows.lock().unwrap()[0]
            .iter()
            .any(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("original request"))));
        assert!(!rt.has_pending_trigger());
    }

    #[test]
    fn restart_resumes_pending_clock_wake_after_a_settled_turn() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        {
            let eja = Eja::open(core_dir.join("subject.db")).unwrap();
            let trigger = eja.notice(Source::Person, "消息").unwrap();
            state::complete_turn(&eja, trigger, "回复").unwrap();
            eja.notice(Source::Clock, "定时唤醒：重启前的提醒").unwrap();
        }
        let core_root = TrustedDir::open(&core_dir).unwrap();
        configured(vec![]).save_models(&core_root).unwrap();

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(data_dir, brain.clone(), Arc::new(|_| {})).unwrap();
        assert!(rt.has_pending_trigger());
        rt.process_pending_turn();
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);
        assert!(!rt.has_pending_trigger());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn registered_check_drives_silent_watch_and_verified_release_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Eja");
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                Ok(tool_reply(
                    "register",
                    "promote_tool",
                    json!({
                        "name":"ready", "file":"status.txt", "equals":"ready"
                    }),
                )),
                Ok(tool_reply(
                    "keep",
                    "keep",
                    json!({"text":"等状态就绪","checker":"ready"}),
                )),
                Ok(tool_reply(
                    "wake",
                    "wake",
                    json!({
                        "delay_seconds":10, "reason":"检查状态", "guard_tool":"ready","max_checks":3
                    }),
                )),
                Ok(text_reply("已守望")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let open = || {
            let sink = events.clone();
            HostRuntime::open_with_brain(
                data.clone(),
                brain.clone(),
                Arc::new(move |event| sink.lock().unwrap().push(event)),
            )
            .unwrap()
        };
        let clock = Arc::new(std::sync::atomic::AtomicI64::new(100));
        let open_subject = || {
            let clock = clock.clone();
            Eja::open(dir.path().join("ai.eja.kernel/subject.db"))
                .unwrap()
                .with_clock(move || Timestamp(clock.load(Ordering::SeqCst)))
        };
        let mut rt = open();
        rt.eja = open_subject();
        rt.save_config(configured(vec![]));
        let status = data.join("status.txt");
        fs::write(&status, "waiting").unwrap();
        rt.handle_user_message("检查状态");
        let concern = rt.eja.concerns(rt.eja.revision().unwrap()).unwrap()[0].id;
        let passive = rt.eja.notice(Source::World, "unrelated").unwrap();
        assert!(!rt.has_pending_trigger());
        assert!(state::release_with_witness(&rt.eja, concern, None).is_err());
        assert!(state::release_with_witness(&rt.eja, concern, Some(passive)).is_err());

        clock.store(110, Ordering::SeqCst);
        rt.tick_timers().unwrap();
        assert_eq!(rt.eja.revision().unwrap(), passive);
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 4);
        drop(rt);

        fs::write(&status, "ready\n").unwrap();
        clock.store(120, Ordering::SeqCst);
        let mut rt = open();
        rt.eja = open_subject();
        let hit = passive + 1;
        brain.replies.lock().unwrap().extend([
            Ok(tool_reply(
                "release",
                "release",
                json!({"concern": concern,"witness_fact_id":hit}),
            )),
            Ok(text_reply("状态已就绪")),
        ]);
        rt.tick_timers().unwrap();
        assert!(matches!(rt.eja.read(hit).unwrap().fact, Fact::Noticed {
            source: Source::World, text
        } if text.contains("ready")));
        assert!(rt
            .eja
            .concerns(rt.eja.revision().unwrap())
            .unwrap()
            .is_empty());
        assert!(state::pending_wakes(&rt.eja).unwrap().is_empty());
        rt.tick_timers().unwrap();
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 6);
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|ev| ev["event"] == "notify")
                .count(),
            1
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn registered_check_rejects_changes_to_rule_and_unsafe_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let data = TrustedDir::open(dir.path()).unwrap();
        let eja = Eja::open(":memory:").unwrap();
        assert!(
            state::register_check(&eja, "unsafe", "../private", Some("yes"), None, None).is_err()
        );
        state::register_check(&eja, "ready", "status.txt", Some("yes"), None, None).unwrap();
        assert!(
            state::register_check(&eja, "ready", "other.txt", Some("yes"), None, None).is_err()
        );
        let check = state::registered_check(&eja, "ready").unwrap();
        assert!(!check.matches(&data).unwrap());
        fs::write(dir.path().join("target"), "yes").unwrap();
        symlink("target", dir.path().join("status.txt")).unwrap();
        assert!(check.matches(&data).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn predicate_checks_wake_dedup_cancel_and_silent_proactive_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Eja");
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                // Turn 1: register `not_equals: ""` check on inbox.md, keep concern, revise concern,
                // schedule two wakes with same reason (second replaces first), schedule a test wake and cancel it,
                // and schedule guarded wake on `inbox_nonempty`.
                Ok(tool_reply(
                    "p1",
                    "promote_tool",
                    json!({
                        "name": "inbox_nonempty",
                        "file": "inbox.md",
                        "not_equals": "",
                        "contains": "紧急"
                    }),
                )),
                Ok(tool_reply(
                    "k1",
                    "keep",
                    json!({ "text": "守望 inbox.md 紧急消息", "checker": "inbox_nonempty" }),
                )),
                Ok(tool_reply(
                    "k2",
                    "keep",
                    json!({
                        "concern": 2,
                        "text": "守望 inbox.md 紧急消息（已激活）\n验收规则：inbox_nonempty"
                    }),
                )),
                Ok(tool_reply(
                    "w1",
                    "wake",
                    json!({ "delay_seconds": 60, "reason": "提醒喝水" }),
                )),
                Ok(tool_reply(
                    "w2",
                    "wake",
                    json!({ "delay_seconds": 120, "reason": "提醒喝水" }),
                )),
                Ok(tool_reply(
                    "w_tmp",
                    "wake",
                    json!({ "delay_seconds": 86400, "reason": "校准测试" }),
                )),
                Ok(tool_reply(
                    "w_cancel",
                    "wake",
                    json!({ "delay_seconds": 0, "reason": "校准测试" }),
                )),
                Ok(tool_reply(
                    "w_guard",
                    "wake",
                    json!({
                        "delay_seconds": 10,
                        "reason": "守望 inbox",
                        "guard_tool": "inbox_nonempty",
                        "max_checks": 5
                    }),
                )),
                Ok(text_reply("已部署静默守望")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = events.clone();
        let clock = Arc::new(std::sync::atomic::AtomicI64::new(1_700_000_000));
        let mut rt = HostRuntime::open_with_brain(
            data.clone(),
            brain.clone(),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
        )
        .unwrap();
        let clock_ref = clock.clone();
        rt.eja = Eja::open(dir.path().join("ai.eja.kernel/subject.db"))
            .unwrap()
            .with_clock(move || Timestamp(clock_ref.load(Ordering::SeqCst)));
        rt.save_config(configured(vec![]));

        let inbox = data.join("inbox.md");
        fs::write(&inbox, "\n").unwrap();
        rt.handle_user_message("开始守望");

        // 1. Concern revision must not duplicate `验收规则：inbox_nonempty`
        let concerns = rt.eja.concerns(rt.eja.revision().unwrap()).unwrap();
        assert_eq!(concerns.len(), 1);
        assert_eq!(
            concerns[0].text.matches("验收规则：inbox_nonempty").count(),
            1
        );
        let concern_id = concerns[0].id;

        // 2. Wakes deduplicated by reason and cancelled via delay_seconds: 0
        let wakes = state::pending_wakes(&rt.eja).unwrap();
        assert_eq!(wakes.len(), 2);
        let drink_wake = wakes.iter().find(|w| w.reason == "提醒喝水").unwrap();
        assert_eq!(drink_wake.fire_at_unix, 1_700_000_120);
        assert!(
            drink_wake.fire_at_display.contains('-'),
            "fire_at_display must include YYYY-MM-DD date: {}",
            drink_wake.fire_at_display
        );

        // 3. Guarded wake at t+10 with empty inbox and t+20 with non-matching inbox stays 100% silent (0 LLM calls)
        let calls_before = brain.seen_windows.lock().unwrap().len();
        clock.store(1_700_000_010, Ordering::SeqCst);
        rt.tick_timers().unwrap();
        fs::write(&inbox, "普通通知：无事发生\n").unwrap();
        clock.store(1_700_000_020, Ordering::SeqCst);
        rt.tick_timers().unwrap();
        assert_eq!(brain.seen_windows.lock().unwrap().len(), calls_before);

        // 4. Plain wake fires at t+120, model replies "（保持沉默）" -> settles without Said fact or notify event!
        brain
            .replies
            .lock()
            .unwrap()
            .push(Ok(text_reply("（保持沉默）")));
        clock.store(1_700_000_120, Ordering::SeqCst);
        // Push guarded wake forward so only the plain wake fires here
        fs::write(&inbox, "普通通知\n").unwrap();
        rt.tick_timers().unwrap();
        assert!(!rt.has_pending_trigger());
        assert!(
            !rt.snapshot()
                .stream
                .iter()
                .any(|item| item.text.contains("保持沉默")),
            "explicit silence marker must not record a Said fact"
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|ev| ev["event"] == "notify")
                .count(),
            0
        );

        // 5. Write matching message to inbox.md; guarded wake fires and model releases concern
        fs::write(&inbox, "紧急：请立即汇报\n").unwrap();
        let next_fact_id = rt.eja.revision().unwrap() + 1;
        brain.replies.lock().unwrap().extend([
            Ok(tool_reply(
                "rel",
                "release",
                json!({ "concern": concern_id, "witness_fact_id": next_fact_id }),
            )),
            Ok(text_reply("收到紧急消息")),
        ]);
        clock.store(1_700_000_130, Ordering::SeqCst);
        rt.tick_timers().unwrap();
        assert!(rt
            .eja
            .concerns(rt.eja.revision().unwrap())
            .unwrap()
            .is_empty());
        assert!(state::pending_wakes(&rt.eja).unwrap().is_empty());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|ev| ev["event"] == "notify" && ev["body"] == "收到紧急消息")
                .count(),
            1
        );
    }

    #[test]
    fn http_mcp_capability_oauth_flow_persists_kernel_token_and_auto_refreshes() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let base_srv = base.clone();
        let active_token = Arc::new(Mutex::new("tok_initial".to_string()));
        let active_token_srv = Arc::clone(&active_token);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);

        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            while !stop_srv.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let body_part = req
                    .split_once("\r\n\r\n")
                    .map(|(_, b)| b)
                    .unwrap_or("")
                    .trim();

                let (status, extra_hdr, resp_body) = if first_line
                    .contains("/.well-known/oauth-protected-resource")
                {
                    (
                        "200 OK",
                        String::new(),
                        json!({
                            "authorization_servers": [format!("{base_srv}/login/oauth")],
                            "scopes_supported": ["repo"]
                        })
                        .to_string(),
                    )
                } else if first_line.contains("/.well-known/oauth-authorization-server") {
                    (
                        "200 OK",
                        String::new(),
                        json!({
                            "issuer": format!("{base_srv}/login/oauth"),
                            "device_authorization_endpoint": format!("{base_srv}/login/device/code"),
                            "token_endpoint": format!("{base_srv}/login/oauth/access_token")
                        })
                        .to_string(),
                    )
                } else if first_line.contains("/login/device/code") {
                    (
                        "200 OK",
                        String::new(),
                        json!({
                            "device_code": "dev_code_1",
                            "user_code": "1111-2222",
                            "verification_uri": format!("{base_srv}/login/device"),
                            "interval": 1,
                            "expires_in": 60
                        })
                        .to_string(),
                    )
                } else if first_line.contains("/login/oauth/access_token") {
                    if body_part.contains("grant_type=refresh_token") {
                        *active_token_srv.lock().unwrap() = "tok_refreshed".into();
                        (
                            "200 OK",
                            String::new(),
                            json!({
                                "access_token": "tok_refreshed",
                                "refresh_token": "ref_2"
                            })
                            .to_string(),
                        )
                    } else {
                        (
                            "200 OK",
                            String::new(),
                            json!({
                                "access_token": "tok_initial",
                                "refresh_token": "ref_1"
                            })
                            .to_string(),
                        )
                    }
                } else if first_line.contains("/mcp") {
                    let expected_auth =
                        format!("authorization: bearer {}", active_token_srv.lock().unwrap());
                    let authorized = req
                        .lines()
                        .any(|line| line.trim().to_ascii_lowercase() == expected_auth);
                    if !authorized {
                        (
                            "401 Unauthorized",
                            format!(
                                "WWW-Authenticate: Bearer resource_metadata=\"{base_srv}/.well-known/oauth-protected-resource/mcp\"\r\n"
                            ),
                            json!({"error": "unauthorized"}).to_string(),
                        )
                    } else {
                        let rpc: Value = serde_json::from_str(body_part).unwrap_or_default();
                        let id = rpc.get("id").cloned().unwrap_or(Value::Null);
                        let method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
                        let body = match method {
                            "initialize" => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "capabilities": {},
                                    "serverInfo": { "name": "mock-http", "version": "1" }
                                }
                            })
                            .to_string(),
                            "tools/list" => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "tools": [{
                                        "name": "whoami",
                                        "description": "Return current identity",
                                        "inputSchema": { "type": "object", "properties": {} }
                                    }]
                                }
                            })
                            .to_string(),
                            "tools/call" => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{
                                        "type": "text",
                                        "text": format!("authed:{}", active_token_srv.lock().unwrap())
                                    }]
                                }
                            })
                            .to_string(),
                            _ => json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string(),
                        };
                        ("200 OK", String::new(), body)
                    }
                } else {
                    ("404 Not Found", String::new(), "{}".to_string())
                };

                let resp = format!(
                    "HTTP/1.1 {status}\r\n{extra_hdr}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(Vec::new()),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(data_dir.clone(), brain.clone(), Arc::new(|_| {}))
                .unwrap();

        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-OAuth-Client-Id".into(), "cli_test".into());
        let cfg = configured(vec![CapabilityConfig {
            id: "gh".into(),
            name: "GitHub".into(),
            enabled: true,
            transport: CapabilityTransport::Http {
                url: format!("{base}/mcp"),
                headers,
            },
        }]);
        rt.save_config(cfg);

        let snap0 = rt.snapshot();
        assert_eq!(snap0.capability_statuses.len(), 2);
        assert!(!snap0.capability_statuses[1].connected);
        assert!(snap0.capability_statuses[1].auth_required);

        rt.oauth_capability("gh".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            rt.tick_timers().unwrap();
            if rt.snapshot().capability_statuses[1].connected {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let snap1 = rt.snapshot();
        assert!(snap1.capability_statuses[1].connected);
        assert!(!snap1.capability_statuses[1].auth_required);
        assert_eq!(snap1.capability_statuses[1].tools.len(), 1);

        let core_oauth_path = dir.path().join("ai.eja.kernel").join("oauth.json");
        assert!(
            core_oauth_path.exists(),
            "oauth.json must live in kernel dir"
        );
        let capabilities_raw = fs::read_to_string(data_dir.join("capabilities.json")).unwrap();
        assert!(
            !capabilities_raw.contains("tok_initial"),
            "capabilities.json must not contain OAuth access token"
        );

        // Rotate server token to force 401 -> automatic refresh_token exchange during tool call!
        *active_token.lock().unwrap() = "rotated_away".into();
        brain.replies.lock().unwrap().extend([
            Ok(tool_reply("c1", "cap_gh_whoami", json!({}))),
            Ok(text_reply("调用成功")),
        ]);
        rt.handle_user_message("测试刷新");

        let seen = brain.seen_windows.lock().unwrap();
        let last_win = seen.last().unwrap();
        assert!(
            last_win
                .iter()
                .any(|m| m.to_string().contains("authed:tok_refreshed")),
            "expected auto-refreshed token in tool output: {last_win:?}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
    }

    #[test]
    fn normalize_reply_text_only_folds_silence_markers_on_proactive_turns() {
        assert_eq!(normalize_reply_text("   ", false), "");
        assert_eq!(normalize_reply_text("沉默", false), "沉默");
        assert_eq!(normalize_reply_text("silent", false), "silent");
        assert_eq!(normalize_reply_text("（保持沉默）", true), "");
        assert_eq!(normalize_reply_text("silent", true), "");
        assert_eq!(normalize_reply_text("提醒内容", true), "提醒内容");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn register_check_reclaims_unreferenced_slots_at_capacity() {
        let eja = Eja::open(":memory:").unwrap();
        for idx in 0..64 {
            state::register_check(
                &eja,
                &format!("old_{idx}"),
                "status.txt",
                Some("ok"),
                None,
                None,
            )
            .unwrap();
        }
        state::register_check(&eja, "fresh", "status.txt", Some("ok"), None, None).unwrap();
        assert!(state::registered_check(&eja, "fresh").is_ok());

        let busy_eja = Eja::open(":memory:").unwrap();
        for idx in 0..64 {
            let name = format!("active_{idx}");
            state::register_check(&busy_eja, &name, "status.txt", Some("ok"), None, None).unwrap();
            state::keep_with_check(&busy_eja, None, &format!("挂念 {idx}"), Some(&name)).unwrap();
        }
        assert!(
            state::register_check(&busy_eja, "overflow", "status.txt", Some("ok"), None, None)
                .is_err()
        );
    }
}
