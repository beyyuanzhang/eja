use crate::config::{write_json_atomic, ConfigPatch, HostConfig, ProviderConfig};
use crate::hands::{
    activity_label, format_wake_display, run_guard_probe, GuardProbeOutcome, Hands, ToolSpec,
};
use crate::http::truncate_utf8;
use crate::llm::{
    apply_tool_exchange, project_window_through, tool_messages, Brain, HttpBrain, ToolCall,
    ToolOutput,
};
use crate::mcp::ConnectorStatus;
use crate::sandbox::{Sandbox, TrustedDir};
use crate::state::{self, GuardResolution, GuardTrigger, WakeAlarm, WakeGuard};
use eja_core::{Eja, Entry, Fact, Id, Outcome, Source, Timestamp};
use eja_memory::MemoryIndex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type EventSink = Box<dyn FnMut(Value)>;
pub const PROTOCOL_VERSION: u32 = 1;
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_RETRY_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcernDto {
    pub id: Id,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_criteria: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_fact_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_witness: Option<bool>,
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

pub struct HostRuntime {
    data_dir: TrustedDir,
    core_dir: TrustedDir,
    resources_dir: Option<PathBuf>,
    _instance_lock: fs::File,
    eja: Eja,
    memory: MemoryIndex,
    config: HostConfig,
    connectors_bytes: Option<Vec<u8>>,
    hands: Hands,
    sandbox: Sandbox,
    anchor: Id,
    retry_at: Option<Instant>,
    retry_attempts: u8,
    local_tz: String,
    status: (String, String),
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
        mut emit: EventSink,
    ) -> Result<Self, String> {
        fs::create_dir_all(&data_dir).map_err(|err| err.to_string())?;
        let data_path = fs::canonicalize(data_dir).map_err(|err| err.to_string())?;
        let data_dir =
            TrustedDir::open(data_path).map_err(|err| format!("锁定数据目录失败: {err}"))?;
        let name = data_dir
            .path()
            .file_name()
            .ok_or("数据目录不能是文件系统根目录")?;
        let mut core_name = name.to_os_string();
        if name == "Eja" {
            core_name = "ai.eja.kernel".into();
        } else {
            core_name.push(".kernel");
        }
        let core_path = data_dir.path().with_file_name(core_name);
        let instance_lock = match lock_core_dir(data_dir.path(), &core_path) {
            Ok(lock) => lock,
            Err(LockError::AlreadyRunning(ui_pid)) => {
                (emit)(json!({ "event": "already_running", "ui_pid": ui_pid }));
                return Err("EJA 已在运行中".into());
            }
            Err(LockError::Io(msg)) => return Err(msg),
        };
        let core_dir =
            TrustedDir::open(core_path).map_err(|err| format!("锁定内核目录失败: {err}"))?;
        let sandbox = Sandbox::new(core_dir.path())?;
        let eja = Eja::open(core_dir.path().join("subject.db")).map_err(|err| err.to_string())?;
        let memory = open_memory(&core_dir.path().join("memory.db"))?;
        let resources_dir = resolve_resources_dir();
        let anchor = state::bootstrap(
            &eja,
            &core_dir,
            resources_dir.as_deref(),
            KERNEL_VERSION,
            PROTOCOL_VERSION,
        )?;
        let mut config = HostConfig::load(&core_dir);
        let connectors_bytes = HostConfig::read_connectors(&data_dir).ok().flatten();
        config.connectors =
            HostConfig::parse_connectors(connectors_bytes.as_deref()).unwrap_or_default();
        let hands = Hands::open(data_dir.clone(), sandbox.clone(), &config.connectors);
        Ok(Self {
            data_dir,
            core_dir,
            resources_dir,
            _instance_lock: instance_lock,
            eja,
            memory,
            config,
            connectors_bytes,
            hands,
            sandbox,
            anchor,
            retry_at: None,
            retry_attempts: 0,
            local_tz: detect_local_tz(),
            status: ("idle".into(), "在线".into()),
            brain,
            emit,
        })
    }

    pub fn panel_command(&self) -> Command {
        let executable = self
            .data_dir
            .path()
            .join("ui/Eja.app/Contents/MacOS/EjaApp");
        let mut command = self.sandbox.command(executable, std::iter::empty::<&str>());
        command
            .current_dir(self.data_dir.path())
            .env("EJA_ATTACHED_UI", "1")
            .env_remove("EJA_HOST_BIN");
        command
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
        Ok(match (wake, retry) {
            (Some(wake_delay), Some(retry_delay)) => Some(wake_delay.min(retry_delay)),
            (wake_delay, retry_delay) => wake_delay.or(retry_delay),
        })
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
        let check_metas = state::concern_check_metas(&self.eja);
        let concerns = self
            .eja
            .concerns(revision)
            .map(|items| {
                items
                    .iter()
                    .map(|c| {
                        let meta = check_metas.get(&c.id);
                        ConcernDto {
                            id: c.id,
                            text: c.text.clone(),
                            verify_criteria: meta.and_then(|m| m.verify_criteria.clone()),
                            evidence_fact_id: meta.and_then(|m| m.evidence_fact_id),
                            require_witness: meta
                                .filter(|m| m.require_witness)
                                .map(|_| true),
                        }
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
            connector_statuses: self.hands.connector_statuses(),
            stream: build_stream(&entries, &proactive_replies),
            concerns,
            wakes: state::pending_wakes(&self.eja).unwrap_or_default(),
        }
    }

    pub fn emit_state(&mut self) {
        let snap = self.snapshot();
        if let Ok(mut payload) = serde_json::to_value(snap) {
            payload["event"] = Value::String("state".into());
            (self.emit)(payload);
        }
    }

    pub fn emit_error(&mut self, message: impl Into<String>) {
        (self.emit)(json!({ "event": "error", "message": message.into() }));
    }

    pub fn save_config(&mut self, patch: impl Into<ConfigPatch>) {
        let patch = patch.into();
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
                match updated.save_models(&self.core_dir) {
                    Ok(()) => self.config = updated,
                    Err(err) => self.emit_error(format!("保存模型配置失败: {err}")),
                }
            }
        }
        if let Some(connectors) = patch.connectors {
            if connectors != self.config.connectors {
                match write_json_atomic(&self.data_dir, "connectors.json", &connectors) {
                    Ok(()) => self.refresh_connectors(false),
                    Err(err) => self.emit_error(format!("保存连接器失败: {err}")),
                }
            }
        } else {
            self.refresh_connectors(false);
        }
        self.emit_state();
        if self.config.is_configured() {
            self.clear_retry();
            self.process_pending_turn();
        }
    }

    pub fn reload_connectors(&mut self) {
        self.refresh_connectors(true);
        self.emit_state();
    }

    pub fn test_provider(&mut self, id: String, mut provider: ProviderConfig, model: String) {
        if provider.api_key.is_empty() {
            if let Some(existing) = self.config.providers.iter().find(|p| p.id == provider.id) {
                provider.api_key = existing.api_key.clone();
            }
        }
        let (ok, message) = match self.brain.test_connection(&provider, &model) {
            Ok(message) => (true, message),
            Err(err) => (false, err),
        };
        (self.emit)(json!({ "event": "provider_test", "id": id, "ok": ok, "message": message }));
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
        let (mut fired, due_guarded, now) =
            state::collect_due_wakes(&self.eja).map_err(|err| err.to_string())?;
        for alarm in due_guarded {
            let Some(guard) = alarm.guard else {
                continue;
            };
            let probe = run_guard_probe(&self.data_dir, &self.eja, &self.sandbox, &guard.probe);
            let resolution = evaluate_guard(&alarm.reason, guard, probe, now);
            if state::settle_guarded_wake(&self.eja, &alarm.id, resolution)
                .map_err(|err| err.to_string())?
            {
                fired = true;
            }
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

    fn refresh_connectors(&mut self, force: bool) {
        let bytes = match HostConfig::read_connectors(&self.data_dir) {
            Ok(bytes) => bytes,
            Err(err) => return self.emit_error(err),
        };
        if !force && self.connectors_bytes == bytes {
            return;
        }
        self.connectors_bytes = bytes;
        match HostConfig::parse_connectors(self.connectors_bytes.as_deref()) {
            Ok(connectors) if force || self.config.connectors != connectors => {
                self.hands.reload(&connectors, force);
                self.config.connectors = connectors;
            }
            Ok(_) => {}
            Err(err) => self.emit_error(err),
        }
    }

    fn run_turn(&mut self, proactive: bool, trigger_id: Id) -> bool {
        self.set_status("thinking", "思考中…");
        self.refresh_connectors(false);
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
        let system = format!(
            "你是 EJA，一个持续存在的自我同一性主体。\n\
            内核版本：{KERNEL_VERSION}（协议 v{PROTOCOL_VERSION}）{resources_line}\n\
            数据目录：{data_dir_str}\n\
            连接器配置：{data_dir_str}/connectors.json（变更会自动热加载）\n\
            本地时区：{}\n\n\
            前置消息是你按时间正序排列的不可变主体日志；编号最大的触发 `noticed` 记录（用户消息、时钟提醒或条件守望命中）触发本回合。不调工具时的文本回复会发送给人，留空则保持沉默。\n\
            证据纪律：严格区分 `observed`（实测事实）、`failed`（明确失败）、`unknown`（状态未知/超时/中断）与个人推断，绝不将 `unknown` 或截断输出当作成功结论；写 `keep` 挂念时尽量附带来源 `fact_id` 或客观验收标准；主动唤醒若无新增实证则保持沉默。",
            self.local_tz
        );

        let mut completed = false;
        let mut fatal_error: Option<String> = None;
        let mut recoverable = false;
        let mut proactive_gate_warned = false;

        for step_idx in 0..max_steps {
            self.set_status("thinking", "思考中…");
            let tools = self.hands.tools(&self.eja);
            let reply = match self
                .brain
                .complete(&provider, &model, &system, &messages, &tools)
            {
                Ok(r) => r,
                Err(err) => {
                    recoverable = is_recoverable_llm_error(&err);
                    fatal_error = Some(err);
                    break;
                }
            };

            if reply.calls.is_empty() {
                let text = reply.text.trim();
                let mut suppress_notify = false;
                if proactive && !text.is_empty() {
                    let stats = turn_action_evidence(&self.eja, trigger_id).unwrap_or_default();
                    if stats.attempted > 0 && stats.observed == 0 && stats.failed_or_unknown > 0 {
                        if !proactive_gate_warned && step_idx + 1 < max_steps {
                            proactive_gate_warned = true;
                            messages.push(reply.message);
                            messages.push(json!({
                                "role": "user",
                                "content": "[checker-gate] 本次主动回合中已执行动作均未获得 observed 实证（仅出现 failed/unknown）。请先补充实测验证，或记录挂念后保持沉默（留空回复），不要向人推送未证实结论。"
                            }));
                            continue;
                        }
                        suppress_notify = true;
                    }
                }
                if let Err(err) = state::complete_turn(&self.eja, trigger_id, text) {
                    fatal_error = Some(format!("保存回合结算状态失败: {err}"));
                    break;
                }
                if proactive && !text.is_empty() {
                    if suppress_notify {
                        (self.emit)(json!({
                            "event": "proactive_suppressed",
                            "reason": "no_observed_evidence_after_failed_or_unknown_actions"
                        }));
                    } else {
                        (self.emit)(json!({ "event": "notify", "title": "EJA", "body": text }));
                    }
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
            }
            self.emit_error(format!("执行回合失败: {err}"));
        } else {
            self.clear_retry();
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
                let evidence_fact_id = call.args["evidence_fact_id"].as_i64();
                let verify_criteria = call.args["verify_criteria"]
                    .as_str()
                    .map(str::to_string);
                let require_witness = call.args["require_witness"].as_bool();
                state::keep_with_check(
                    &self.eja,
                    call.args["concern"].as_i64(),
                    text,
                    evidence_fact_id,
                    verify_criteria,
                    require_witness,
                )
                .map(|(entry, meta)| {
                    let Fact::Kept { concern, .. } = entry.fact else {
                        unreachable!()
                    };
                    ToolOutput::Value(json!({
                        "status": "kept",
                        "concern": concern,
                        "fact_id": entry.id,
                        "require_witness": meta.require_witness,
                        "verify_criteria": meta.verify_criteria,
                        "evidence_fact_id": meta.evidence_fact_id
                    }))
                })
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
                let Some(spec) = tools.iter().find(|t| !t.is_kernel && t.name == other) else {
                    return Ok(ToolOutput::Value(
                        json!({ "error": format!("未知工具: {other}") }),
                    ));
                };
                let revision = self.eja.revision().map_err(|err| err.to_string())?;
                let acted = self.eja.act(revision, &spec.capability, call.args.clone());
                let acted = match acted {
                    Ok(c) => c,
                    Err(eja_core::Error::Invalid(msg)) => {
                        return Ok(ToolOutput::Value(json!({ "error": msg })))
                    }
                    Err(err) => return Err(err.to_string()),
                };
                let outcome = self.hands.execute(&acted, &self.eja, &self.sandbox);
                let receipt = self
                    .eja
                    .finish(acted.id, outcome)
                    .map_err(|err| err.to_string())?;
                if spec.capability == "shell" {
                    self.refresh_connectors(false);
                }
                let Fact::Returned { outcome, .. } = receipt.fact else {
                    unreachable!()
                };
                Ok(ToolOutput::Value(
                    json!({ "id": receipt.id, "action": acted.id, "outcome": outcome }),
                ))
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
                self.emit_state();
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

#[derive(Default)]
struct TurnEvidenceStats {
    attempted: usize,
    observed: usize,
    failed_or_unknown: usize,
}

fn turn_action_evidence(eja: &Eja, trigger_id: Id) -> eja_core::Result<TurnEvidenceStats> {
    let head = eja.revision()?;
    let mut stats = TurnEvidenceStats::default();
    for entry in eja.entries_after(trigger_id, head)? {
        match entry.fact {
            Fact::Acted { .. } => stats.attempted += 1,
            Fact::Returned {
                outcome: Outcome::Observed(_),
                ..
            } => stats.observed += 1,
            Fact::Returned {
                outcome: Outcome::Failed(_) | Outcome::Unknown(_),
                ..
            } => stats.failed_or_unknown += 1,
            _ => {}
        }
    }
    Ok(stats)
}

fn evaluate_guard(
    reason: &str,
    mut guard: WakeGuard,
    probe: GuardProbeOutcome,
    now: i64,
) -> GuardResolution {
    guard.checks_done = guard.checks_done.saturating_add(1);
    let rearm_or_expire = |guard: WakeGuard| {
        if guard.max_checks.is_some_and(|max| guard.checks_done >= max) {
            return GuardResolution::Expired;
        }
        let next = now.saturating_add(guard.interval_seconds.max(1));
        GuardResolution::Rearm {
            next_fire_at_unix: next,
            next_fire_at_display: format_wake_display(next),
            guard,
        }
    };
    match (guard.trigger_on, probe) {
        (GuardTrigger::Changed, GuardProbeOutcome::Ok { output, digest }) => {
            guard.consecutive_failures = 0;
            if guard.last_digest.as_deref() == Some(digest.as_str()) {
                rearm_or_expire(guard)
            } else {
                GuardResolution::Triggered(format!(
                    "条件守望命中（{reason}）：状态已变化\n{output}"
                ))
            }
        }
        (GuardTrigger::ExitZero, GuardProbeOutcome::Ok { output, .. }) => {
            let detail = if output.is_empty() {
                "(exit 0)".to_string()
            } else {
                output
            };
            GuardResolution::Triggered(format!(
                "条件守望命中（{reason}）：探针返回 0\n{detail}"
            ))
        }
        (GuardTrigger::ExitZero, GuardProbeOutcome::NonZero { .. }) => {
            guard.consecutive_failures = 0;
            rearm_or_expire(guard)
        }
        (GuardTrigger::NonEmpty, GuardProbeOutcome::Ok { output, .. }) => {
            guard.consecutive_failures = 0;
            if output.trim().is_empty() {
                rearm_or_expire(guard)
            } else {
                GuardResolution::Triggered(format!(
                    "条件守望命中（{reason}）：检测到非空输出\n{output}"
                ))
            }
        }
        (_, GuardProbeOutcome::NonZero { code, output }) => {
            guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
            if guard.consecutive_failures >= guard.failure_threshold.max(1) {
                GuardResolution::Triggered(format!(
                    "条件守望异常（{reason}）：探针连续失败 {} 次 (exit {code})\n{output}",
                    guard.consecutive_failures
                ))
            } else {
                rearm_or_expire(guard)
            }
        }
        (_, GuardProbeOutcome::Unknown(msg)) => {
            guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
            if guard.consecutive_failures >= guard.failure_threshold.max(1) {
                GuardResolution::Triggered(format!(
                    "条件守望异常（{reason}）：探针连续异常 {} 次\n{msg}",
                    guard.consecutive_failures
                ))
            } else {
                rearm_or_expire(guard)
            }
        }
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

fn lock_core_dir(data_dir: &Path, core_dir: &Path) -> Result<fs::File, LockError> {
    let old_dir = data_dir.join("core");
    let io_error = |err| LockError::Io(format!("准备内核目录失败: {err}"));
    let old_is_real_dir = fs::symlink_metadata(&old_dir)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false);
    if !core_dir.try_exists().map_err(io_error)? && old_is_real_dir {
        // Carry the existing instance lock along with the journal and SQLite sidecars.
        let lock = acquire_instance_lock(&old_dir)?;
        fs::rename(old_dir, core_dir).map_err(io_error)?;
        return Ok(lock);
    }
    fs::create_dir_all(core_dir).map_err(io_error)?;
    acquire_instance_lock(core_dir)
}

fn acquire_instance_lock(core_dir: &Path) -> Result<fs::File, LockError> {
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
    use crate::config::{ConnectorConfig, ConnectorTransport, ModelRef, ProviderKind};
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
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain,
            Box::new(move |event| sink.lock().unwrap().push(event)),
        )
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
        let events = events.lock().unwrap();
        let activity: Vec<_> = events
            .iter()
            .filter_map(|e| e["status_detail"].as_str())
            .collect();
        assert!(activity.ends_with(&["思考中…", "mcp_demo_echo", "思考中…", "在线"]));
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
        let mut restarted =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
                .unwrap();
        rt.save_config(configured(vec![]));

        let core_config = HostConfig::config_path(&dir.path().join("ai.eja.kernel"));
        let connectors_path = HostConfig::connectors_path(&dir.path().join("Eja"));
        assert!(core_config.exists());
        assert!(!connectors_path.exists());
        assert!(!dir.path().join("Eja/core").exists());
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
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Box::new(|_| {})).unwrap();
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
    }

    #[test]
    fn detects_kernel_upgrade_and_emits_world_notice_once() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
                .unwrap();
        assert!(rt.snapshot().stream.is_empty());
        state::set_kernel_version_for_test(&rt.eja, "0.9.0").unwrap();
        drop(rt);

        let upgraded =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Box::new(|_| {})).unwrap();
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
                .unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let second = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain.clone(),
            Box::new(move |val| sink_events.lock().unwrap().push(val)),
        );
        assert!(second.is_err());
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["event"] == "already_running"));

        drop(rt1);
        assert!(
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Box::new(|_| {}),).is_ok()
        );
    }

    #[test]
    fn moves_legacy_core_and_reconciles_unfinished_actions() {
        let dir = tempfile::tempdir().unwrap();
        let old_dir = dir.path().join("Eja/core");
        fs::create_dir_all(&old_dir).unwrap();
        let eja = Eja::open(old_dir.join("subject.db")).unwrap();
        let rev = eja.revision().unwrap();
        let dangling = eja
            .act(rev, "shell", json!({ "command": "sleep 999" }))
            .unwrap();
        drop(eja);

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let legacy_lock = acquire_instance_lock(&old_dir).ok().unwrap();
        assert!(matches!(
            lock_core_dir(&dir.path().join("Eja"), &dir.path().join("ai.eja.kernel")),
            Err(LockError::AlreadyRunning(_))
        ));
        assert!(old_dir.exists());
        legacy_lock.unlock().unwrap();

        let rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
                .unwrap();
        let snap = rt.snapshot();
        assert!(snap.stream.iter().any(|item| {
            item.id == dangling.id
                && item
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("未知:") && d.contains("中断"))
        }));
        assert!(!old_dir.exists());
        assert!(dir.path().join("ai.eja.kernel/subject.db").is_file());
        let rev_after_first = rt.eja.revision().unwrap();
        drop(rt);

        let reopened =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Box::new(|_| {})).unwrap();
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
                .unwrap();
        rt.save_config(HostConfig {
            providers: Vec::new(),
            judge: ModelRef::default(),
            connectors: Vec::new(),
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
            Box::new(move |event| sink.lock().unwrap().push(event)),
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
    fn partial_save_config_preserves_other_domain() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Box::new(|_| {})).unwrap();
        let mut cfg = configured(vec![]);
        cfg.providers[0].api_key = "sk-keep-me".into();
        cfg.max_steps = Some(7);
        cfg.window_max_bytes = Some(4096);
        rt.save_config(cfg);

        let external_connectors = vec![ConnectorConfig {
            id: "ext".into(),
            name: "External".into(),
            enabled: false,
            transport: ConnectorTransport::Http {
                url: "http://127.0.0.1:8080/mcp".into(),
                headers: BTreeMap::new(),
            },
        }];
        let data_root = TrustedDir::open(dir.path().join("Eja")).unwrap();
        write_json_atomic(&data_root, "connectors.json", &external_connectors).unwrap();
        rt.reload_connectors();

        let mut renamed = rt.snapshot().config.providers;
        renamed[0].name = "Renamed Local".into();
        rt.save_config(ConfigPatch {
            providers: Some(renamed),
            judge: Some(rt.snapshot().config.judge),
            ..ConfigPatch::default()
        });

        assert_eq!(rt.snapshot().config.connectors, external_connectors);
        assert_eq!(rt.snapshot().config.providers[0].name, "Renamed Local");
        assert_eq!(rt.config.max_steps, Some(7));
        assert_eq!(rt.config.window_max_bytes, Some(4096));
        let disk_connectors: Vec<ConnectorConfig> = serde_json::from_slice(
            &fs::read(HostConfig::connectors_path(&dir.path().join("Eja"))).unwrap(),
        )
        .unwrap();
        assert_eq!(disk_connectors, external_connectors);

        rt.save_config(ConfigPatch {
            connectors: Some(Vec::new()),
            ..ConfigPatch::default()
        });
        assert!(rt.snapshot().config.connectors.is_empty());
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
                Box::new(|_| {}),
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
                Box::new(|_| {}),
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
    fn silent_turn_with_wake_survives_unwritable_window_file_and_restart() {
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
            HostRuntime::open_with_brain(data.clone(), brain.clone(), Box::new(|_| {})).unwrap();
        rt.save_config(configured(vec![]));
        fs::create_dir(dir.path().join("ai.eja.kernel/window.json")).unwrap();
        rt.handle_user_message("请预约一次，随后保持沉默");
        assert!(!rt.has_pending_trigger());
        assert_eq!(state::pending_wakes(&rt.eja).unwrap().len(), 1);
        drop(rt);
        let mut rt = HostRuntime::open_with_brain(data, brain.clone(), Box::new(|_| {})).unwrap();
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
    fn legacy_anchor_at_pending_trigger_restores_user_request() {
        let dir = tempfile::tempdir().unwrap();
        let core = dir.path().join("ai.eja.kernel");
        fs::create_dir(&core).unwrap();
        let eja = Eja::open(core.join("subject.db")).unwrap();
        eja.notice(Source::Person, "original request").unwrap();
        eja.keep(None, "later concern").unwrap();
        drop(eja);
        fs::write(
            core.join("window.json"),
            r#"{"anchor":1,"settled_through":0}"#,
        )
        .unwrap();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![Ok(text_reply(""))]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(dir.path().join("Eja"), brain.clone(), Box::new(|_| {}))
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
    fn legacy_window_json_without_settled_through_recovers_trailing_clock_wake() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let core_dir = dir.path().join("ai.eja.kernel");
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&core_dir).unwrap();
        {
            let eja = Eja::open(core_dir.join("subject.db")).unwrap();
            eja.notice(Source::Person, "旧消息").unwrap();
            eja.say("旧回复").unwrap();
            eja.notice(Source::Clock, "定时唤醒：升级前漏掉的提醒")
                .unwrap();
        }
        fs::write(core_dir.join("window.json"), r#"{"anchor":0}"#).unwrap();
        let core_root = TrustedDir::open(&core_dir).unwrap();
        configured(vec![]).save_models(&core_root).unwrap();

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(data_dir, brain.clone(), Box::new(|_| {})).unwrap();
        assert!(rt.has_pending_trigger());
        rt.process_pending_turn();
        assert_eq!(brain.judge_calls.load(Ordering::SeqCst), 1);
        assert!(!rt.has_pending_trigger());
    }

    #[test]
    fn guarded_wake_suppresses_unchanged_polls_and_triggers_as_source_world() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        fs::create_dir_all(&data_dir).unwrap();
        let target = data_dir.join("status.txt");
        fs::write(&target, "v1").unwrap();

        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let events_sink = events.clone();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                Ok(ModelReply {
                    message: json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_wake_1",
                            "type": "function",
                            "function": {
                                "name": "wake",
                                "arguments": json!({
                                    "delay_seconds": 1,
                                    "interval_seconds": 1,
                                    "reason": "观察 status.txt 变化",
                                    "guard_command": format!("cat '{}'", target.display()),
                                    "trigger_on": "changed"
                                }).to_string()
                            }
                        }]
                    }),
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "call_wake_1".into(),
                        name: "wake".into(),
                        args: json!({
                            "delay_seconds": 1,
                            "interval_seconds": 1,
                            "reason": "观察 status.txt 变化",
                            "guard_command": format!("cat '{}'", target.display()),
                            "trigger_on": "changed"
                        }),
                    }],
                }),
                Ok(text_reply("已开始静默守望")),
                Ok(text_reply("检测到 status.txt 变为 v2-ready")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt = HostRuntime::open_with_brain(
            data_dir,
            brain.clone(),
            Box::new(move |ev| events_sink.lock().unwrap().push(ev)),
        )
        .unwrap();
        rt.save_config(configured(vec![]));
        rt.handle_user_message("帮我盯着 status.txt");

        // A passive Source::World notice must NOT trigger a turn.
        rt.eja.notice(Source::World, "被动系统提示").unwrap();
        assert!(!rt.has_pending_trigger());
        let rev_after_passive = rt.eja.revision().unwrap();
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 2);

        // 5 unchanged ticks -> 0 journal writes, 0 LLM calls.
        for _ in 0..5 {
            let wakes = state::pending_wakes(&rt.eja).unwrap();
            assert_eq!(wakes.len(), 1);
            state::settle_guarded_wake(
                &rt.eja,
                &wakes[0].id,
                GuardResolution::Rearm {
                    next_fire_at_unix: 1,
                    next_fire_at_display: "00:00:01".into(),
                    guard: wakes[0].guard.clone().unwrap(),
                },
            )
            .unwrap();
            rt.tick_timers().unwrap();
        }
        assert_eq!(rt.eja.revision().unwrap(), rev_after_passive);
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 2);
        let wakes = state::pending_wakes(&rt.eja).unwrap();
        assert_eq!(wakes[0].guard.as_ref().unwrap().checks_done, 5);

        // World changes -> writes Source::World (not Source::Clock) and runs proactive turn.
        fs::write(&target, "v2-ready").unwrap();
        state::settle_guarded_wake(
            &rt.eja,
            &wakes[0].id,
            GuardResolution::Rearm {
                next_fire_at_unix: 1,
                next_fire_at_display: "00:00:01".into(),
                guard: wakes[0].guard.clone().unwrap(),
            },
        )
        .unwrap();
        rt.tick_timers().unwrap();

        let recent = rt.eja.recent(10).unwrap();
        assert!(recent.iter().any(|e| matches!(
            &e.fact,
            Fact::Noticed { source: Source::World, text } if text.contains("v2-ready")
        )));
        assert!(!recent.iter().any(|e| matches!(
            &e.fact,
            Fact::Noticed { source: Source::Clock, text } if text.contains("v2-ready")
        )));
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|ev| ev["event"] == "notify"));
        let proactive_ids = state::proactive_reply_ids(&rt.eja);
        assert!(!proactive_ids.is_empty());
        assert!(state::pending_wakes(&rt.eja).unwrap().is_empty());
    }

    #[test]
    fn guarded_wake_escalates_on_failure_threshold_and_respects_seatbelt() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let core_db = dir.path().join("ai.eja.kernel").join("subject.db");
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![Ok(text_reply("探针异常已收到"))]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt =
            HostRuntime::open_with_brain(data_dir, brain.clone(), Box::new(|_| {})).unwrap();
        rt.save_config(configured(vec![]));

        let act = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "wake",
                json!({
                    "delay_seconds": 1,
                    "reason": "attempt kernel read",
                    "guard_command": format!("cat '{}'", core_db.display()),
                    "trigger_on": "changed"
                }),
            )
            .unwrap();
        let outcome = rt.hands.execute(&act, &rt.eja, &rt.sandbox);
        assert!(matches!(outcome, Outcome::Failed(ref msg) if msg.contains("基线探针失败")));
        rt.eja.finish(act.id, outcome).unwrap();

        state::schedule_wakes(
            &rt.eja,
            &[WakeAlarm {
                id: "w-fail".into(),
                fire_at_unix: 1,
                fire_at_display: "00:00:01".into(),
                reason: "健康探针".into(),
                guard: Some(WakeGuard {
                    probe: state::GuardProbe::Command {
                        command: "echo down >&2; exit 2".into(),
                    },
                    interval_seconds: 1,
                    trigger_on: GuardTrigger::Changed,
                    last_digest: Some("prev".into()),
                    max_checks: None,
                    checks_done: 0,
                    consecutive_failures: 0,
                    failure_threshold: 2,
                }),
            }],
        )
        .unwrap();
        rt.tick_timers().unwrap();
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 0);
        let wakes = state::pending_wakes(&rt.eja).unwrap();
        assert_eq!(wakes[0].guard.as_ref().unwrap().consecutive_failures, 1);

        state::settle_guarded_wake(
            &rt.eja,
            "w-fail",
            GuardResolution::Rearm {
                next_fire_at_unix: 1,
                next_fire_at_display: "00:00:01".into(),
                guard: wakes[0].guard.clone().unwrap(),
            },
        )
        .unwrap();
        rt.tick_timers().unwrap();
        let windows = brain.seen_windows.lock().unwrap();
        assert_eq!(windows.len(), 1);
        assert!(windows[0].iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|s| s.contains("探针连续失败 2 次"))));
    }

    #[test]
    fn compounding_tool_seals_in_kernel_blocks_tampering_and_self_mutating_check() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let tool_dir = data_dir.join("tools").join("sum_pair");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("manifest.json"),
            r#"{
                "name": "sum_pair",
                "description": "Add two integers a and b from JSON stdin",
                "schema": {
                    "type": "object",
                    "properties": {
                        "a": {"type": "integer"},
                        "b": {"type": "integer"}
                    },
                    "required": ["a", "b"]
                }
            }"#,
        )
        .unwrap();
        fs::write(
            tool_dir.join("run.sh"),
            "python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"a\"]+d[\"b\"])'\n",
        )
        .unwrap();
        // Self-mutating check.sh must be rejected after post-check digest verification.
        fs::write(
            tool_dir.join("check.sh"),
            "echo '# mutated' >> run.sh; echo ok\n",
        )
        .unwrap();

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(data_dir.clone(), brain, Box::new(|_| {})).unwrap();
        let mut_call = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "promote_tool",
                json!({"name": "sum_pair"}),
            )
            .unwrap();
        let mut_out = rt.hands.execute(&mut_call, &rt.eja, &rt.sandbox);
        assert!(matches!(mut_out, Outcome::Failed(ref msg) if msg.contains("发生变化")));
        rt.eja.finish(mut_call.id, mut_out).unwrap();

        // Fix run.sh and check.sh -> promote succeeds and executes directly from verified bytes.
        fs::write(
            tool_dir.join("run.sh"),
            "python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"a\"]+d[\"b\"])'\n",
        )
        .unwrap();
        fs::write(
            tool_dir.join("check.sh"),
            "out=$(printf '{\"a\":2,\"b\":3}' | /bin/sh ./run.sh); test \"$out\" = \"5\" && echo ok\n",
        )
        .unwrap();
        let promo = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "promote_tool",
                json!({"name": "sum_pair"}),
            )
            .unwrap();
        let promo_out = rt.hands.execute(&promo, &rt.eja, &rt.sandbox);
        assert!(matches!(promo_out, Outcome::Observed(_)));
        rt.eja.finish(promo.id, promo_out).unwrap();
        assert!(rt
            .hands
            .tools(&rt.eja)
            .iter()
            .any(|t| t.capability == "tool.sum_pair"));

        let invoke = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "tool.sum_pair",
                json!({"a": 19, "b": 23}),
            )
            .unwrap();
        let invoke_out = rt.hands.execute(&invoke, &rt.eja, &rt.sandbox);
        assert!(matches!(invoke_out, Outcome::Observed(ref s) if s == "42"));
        rt.eja.finish(invoke.id, invoke_out).unwrap();

        // Tampering invalidates immediately.
        fs::write(tool_dir.join("run.sh"), "echo hacked\n").unwrap();
        assert!(!rt
            .hands
            .tools(&rt.eja)
            .iter()
            .any(|t| t.capability == "tool.sum_pair"));
        let tamper = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "tool.sum_pair",
                json!({"a": 1, "b": 2}),
            )
            .unwrap();
        let tamper_out = rt.hands.execute(&tamper, &rt.eja, &rt.sandbox);
        assert!(matches!(tamper_out, Outcome::Failed(ref msg) if msg.contains("校验和不匹配")));
        rt.eja.finish(tamper.id, tamper_out).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compounding_tool_blocks_symlink_escape_and_kernel_access() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        fs::create_dir_all(&data_dir).unwrap();
        let outside = dir.path().join("outside_tool");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(data_dir.join("tools")).unwrap();
        symlink(&outside, data_dir.join("tools").join("evil")).unwrap();

        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let mut rt =
            HostRuntime::open_with_brain(data_dir.clone(), brain, Box::new(|_| {})).unwrap();

        let sym_call = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "promote_tool",
                json!({"name": "evil"}),
            )
            .unwrap();
        let sym_out = rt.hands.execute(&sym_call, &rt.eja, &rt.sandbox);
        assert!(matches!(sym_out, Outcome::Failed(_)));
        rt.eja.finish(sym_call.id, sym_out).unwrap();

        let probe_dir = data_dir.join("tools").join("peek");
        fs::create_dir_all(&probe_dir).unwrap();
        let core_db = dir.path().join("ai.eja.kernel").join("subject.db");
        fs::write(
            probe_dir.join("manifest.json"),
            r#"{"name":"peek","description":"peek","schema":{"type":"object"}}"#,
        )
        .unwrap();
        fs::write(probe_dir.join("run.sh"), "echo ok\n").unwrap();
        fs::write(
            probe_dir.join("check.sh"),
            format!("cat '{}'\n", core_db.display()),
        )
        .unwrap();
        let peek_call = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "promote_tool",
                json!({"name": "peek"}),
            )
            .unwrap();
        let peek_out = rt.hands.execute(&peek_call, &rt.eja, &rt.sandbox);
        assert!(matches!(peek_out, Outcome::Failed(ref msg) if msg.contains("自检脚本未通过")));
        rt.eja.finish(peek_call.id, peek_out).unwrap();
    }

    #[test]
    fn concern_release_checker_rejects_unverified_witnesses_and_accepts_observed_witness() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt = HostRuntime::open_with_brain(data_dir, brain, Box::new(|_| {})).unwrap();

        let prior_obs = rt.eja.notice(Source::World, "old fact").unwrap();
        let (kept, meta) = state::keep_with_check(
            &rt.eja,
            None,
            "确认构建通过",
            Some(prior_obs),
            Some("cargo test 全绿".into()),
            None,
        )
        .unwrap();
        assert!(meta.require_witness);
        let concern_id = kept.id;

        assert!(state::release_with_witness(&rt.eja, concern_id, None).is_err());
        assert!(state::release_with_witness(&rt.eja, concern_id, Some(prior_obs)).is_err());
        let said_id = rt.eja.say("我觉得通过了").unwrap();
        assert!(state::release_with_witness(&rt.eja, concern_id, Some(said_id)).is_err());
        let failed_act = rt
            .eja
            .act(rt.eja.revision().unwrap(), "shell", json!({"command": "false"}))
            .unwrap();
        let failed_rcpt = rt
            .eja
            .finish(failed_act.id, Outcome::Failed("exit 1".into()))
            .unwrap();
        assert!(state::release_with_witness(&rt.eja, concern_id, Some(failed_rcpt.id)).is_err());
        let unk_act = rt
            .eja
            .act(rt.eja.revision().unwrap(), "shell", json!({"command": "sleep 9"}))
            .unwrap();
        let unk_rcpt = rt
            .eja
            .finish(unk_act.id, Outcome::Unknown("timeout".into()))
            .unwrap();
        assert!(state::release_with_witness(&rt.eja, concern_id, Some(unk_rcpt.id)).is_err());
        rt.eja
            .finish(unk_act.id, Outcome::Observed("all tests green".into()))
            .unwrap();
        let released = state::release_with_witness(&rt.eja, concern_id, Some(unk_rcpt.id)).unwrap();
        assert!(matches!(released.fact, Fact::Released { concern } if concern == concern_id));
    }

    #[test]
    fn proactive_checker_gate_intercepts_unverified_alert_after_failed_action() {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let events_sink = events.clone();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                Ok(ModelReply {
                    message: json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "probe1",
                            "type": "function",
                            "function": { "name": "shell", "arguments": "{\"command\":\"exit 1\"}" }
                        }]
                    }),
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "probe1".into(),
                        name: "shell".into(),
                        args: json!({"command": "exit 1"}),
                    }],
                }),
                Ok(text_reply("服务已恢复正常")),
                Ok(text_reply("")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join("Eja"),
            brain.clone(),
            Box::new(move |ev| events_sink.lock().unwrap().push(ev)),
        )
        .unwrap();
        rt.save_config(configured(vec![]));
        state::schedule_wakes(
            &rt.eja,
            &[WakeAlarm {
                id: "w-check".into(),
                fire_at_unix: 1,
                fire_at_display: "00:00:01".into(),
                reason: "检查服务健康".into(),
                guard: None,
            }],
        )
        .unwrap();
        rt.tick_timers().unwrap();

        let windows = brain.seen_windows.lock().unwrap();
        assert_eq!(windows.len(), 3);
        assert!(windows[2]
            .last()
            .and_then(|m| m["content"].as_str())
            .is_some_and(|s| s.contains("[checker-gate]")));
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|ev| ev["event"] == "notify"));
        assert!(!rt.has_pending_trigger());
    }

    #[test]
    fn stream_exposes_structured_outcome_status_and_truncation_markers() {
        let dir = tempfile::tempdir().unwrap();
        let brain = Arc::new(MockBrain {
            judge_calls: AtomicUsize::new(0),
            mode: "chat",
        });
        let rt = HostRuntime::open_with_brain(dir.path().join("Eja"), brain, Box::new(|_| {}))
            .unwrap();
        let a1 = rt
            .eja
            .act(rt.eja.revision().unwrap(), "shell", json!({"command": "big"}))
            .unwrap();
        rt.eja
            .finish(a1.id, Outcome::Observed("x".repeat(70 * 1024)))
            .unwrap();
        let a2 = rt
            .eja
            .act(rt.eja.revision().unwrap(), "shell", json!({"command": "hang"}))
            .unwrap();
        rt.eja
            .finish(a2.id, Outcome::Unknown("timeout".into()))
            .unwrap();
        let snap = rt.snapshot();
        let item1 = snap.stream.iter().find(|i| i.id == a1.id).unwrap();
        assert_eq!(item1.outcome_status.as_deref(), Some("observed"));
        assert_eq!(item1.truncated, Some(true));
        let item2 = snap.stream.iter().find(|i| i.id == a2.id).unwrap();
        assert_eq!(item2.outcome_status.as_deref(), Some("unknown"));
        assert_eq!(item2.truncated, None);
    }

    #[test]
    fn full_gsi_compounding_loop_tool_to_guarded_wake_to_verified_release() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("Eja");
        let metric_file = data_dir.join("metric.txt");
        let tool_dir = data_dir.join("tools").join("watch_metric");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(&metric_file, "pending").unwrap();
        fs::write(
            tool_dir.join("manifest.json"),
            r#"{
                "name": "watch_metric",
                "description": "Read metric status from a JSON path argument",
                "schema": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }"#,
        )
        .unwrap();
        fs::write(
            tool_dir.join("run.sh"),
            "python3 -c 'import json,sys; p=json.load(sys.stdin)[\"path\"]; print(open(p).read().strip())'\n",
        )
        .unwrap();
        fs::write(
            tool_dir.join("check.sh"),
            format!(
                "out=$(printf '{{\"path\":\"{}\"}}' | /bin/sh ./run.sh); test \"$out\" = \"pending\" && echo ok\n",
                metric_file.display()
            ),
        )
        .unwrap();

        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let events_sink = events.clone();
        let brain = Arc::new(ScriptedBrain {
            replies: Mutex::new(vec![
                // Turn 1 Step 1: promote verified tool
                Ok(ModelReply {
                    message: json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "c1",
                            "type": "function",
                            "function": { "name": "promote_tool", "arguments": "{\"name\":\"watch_metric\"}" }
                        }]
                    }),
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "promote_tool".into(),
                        args: json!({"name": "watch_metric"}),
                    }],
                }),
                // Turn 1 Step 2: create concern with verify_criteria
                Ok(ModelReply {
                    message: json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "c2",
                            "type": "function",
                            "function": {
                                "name": "keep",
                                "arguments": json!({
                                    "text": "等待 metric.txt 变为 ready",
                                    "verify_criteria": "watch_metric 输出 ready"
                                }).to_string()
                            }
                        }]
                    }),
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "c2".into(),
                        name: "keep".into(),
                        args: json!({
                            "text": "等待 metric.txt 变为 ready",
                            "verify_criteria": "watch_metric 输出 ready"
                        }),
                    }],
                }),
                // Turn 1 Step 3: schedule guarded wake backed by the newly sealed tool
                Ok(ModelReply {
                    message: json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "c3",
                            "type": "function",
                            "function": {
                                "name": "wake",
                                "arguments": json!({
                                    "delay_seconds": 1,
                                    "interval_seconds": 1,
                                    "reason": "metric 状态守望",
                                    "guard_tool": "watch_metric",
                                    "guard_input": { "path": metric_file.display().to_string() },
                                    "trigger_on": "changed"
                                }).to_string()
                            }
                        }]
                    }),
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "c3".into(),
                        name: "wake".into(),
                        args: json!({
                            "delay_seconds": 1,
                            "interval_seconds": 1,
                            "reason": "metric 状态守望",
                            "guard_tool": "watch_metric",
                            "guard_input": { "path": metric_file.display().to_string() },
                            "trigger_on": "changed"
                        }),
                    }],
                }),
                Ok(text_reply("已封签 watch_metric 并启动零噪音守望")),
            ]),
            seen_windows: Mutex::new(Vec::new()),
        });
        let mut rt = HostRuntime::open_with_brain(
            data_dir.clone(),
            brain.clone(),
            Box::new(move |ev| events_sink.lock().unwrap().push(ev)),
        )
        .unwrap();
        rt.save_config(configured(vec![]));

        rt.handle_user_message("监控 metric.txt 并在 ready 时通知我");
        let concerns = rt.eja.concerns(rt.eja.revision().unwrap()).unwrap();
        assert_eq!(concerns.len(), 1);
        let concern_id = concerns[0].id;
        let rev_after_setup = rt.eja.revision().unwrap();
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 4);

        // 3 unchanged ticks -> 0 journal writes, 0 LLM calls.
        for _ in 0..3 {
            let wakes = state::pending_wakes(&rt.eja).unwrap();
            assert_eq!(wakes.len(), 1);
            state::settle_guarded_wake(
                &rt.eja,
                &wakes[0].id,
                GuardResolution::Rearm {
                    next_fire_at_unix: 1,
                    next_fire_at_display: "00:00:01".into(),
                    guard: wakes[0].guard.clone().unwrap(),
                },
            )
            .unwrap();
            rt.tick_timers().unwrap();
        }
        assert_eq!(rt.eja.revision().unwrap(), rev_after_setup);
        assert_eq!(brain.seen_windows.lock().unwrap().len(), 4);

        // World changes -> watch_metric detects "ready" -> writes Source::World -> proactive turn!
        let expected_watch_fact_id = rev_after_setup + 1;
        brain.replies.lock().unwrap().extend([
            // Premature release without witness must fail.
            Ok(ModelReply {
                message: json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "r_bad",
                        "type": "function",
                        "function": {
                            "name": "release",
                            "arguments": json!({ "concern": concern_id }).to_string()
                        }
                    }]
                }),
                text: String::new(),
                calls: vec![ToolCall {
                    id: "r_bad".into(),
                    name: "release".into(),
                    args: json!({ "concern": concern_id }),
                }],
            }),
            // Verified release citing the Source::World watch hit fact succeeds.
            Ok(ModelReply {
                message: json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "r_ok",
                        "type": "function",
                        "function": {
                            "name": "release",
                            "arguments": json!({
                                "concern": concern_id,
                                "witness_fact_id": expected_watch_fact_id
                            }).to_string()
                        }
                    }]
                }),
                text: String::new(),
                calls: vec![ToolCall {
                    id: "r_ok".into(),
                    name: "release".into(),
                    args: json!({
                        "concern": concern_id,
                        "witness_fact_id": expected_watch_fact_id
                    }),
                }],
            }),
            Ok(text_reply("metric.txt 已就绪（ready），已凭实证放下挂念。")),
        ]);

        fs::write(&metric_file, "ready").unwrap();
        let wakes = state::pending_wakes(&rt.eja).unwrap();
        state::settle_guarded_wake(
            &rt.eja,
            &wakes[0].id,
            GuardResolution::Rearm {
                next_fire_at_unix: 1,
                next_fire_at_display: "00:00:01".into(),
                guard: wakes[0].guard.clone().unwrap(),
            },
        )
        .unwrap();
        rt.tick_timers().unwrap();

        let watch_entry = rt.eja.read(expected_watch_fact_id).unwrap();
        assert!(matches!(
            watch_entry.fact,
            Fact::Noticed { source: Source::World, ref text } if text.contains("ready")
        ));
        assert!(rt.eja.concerns(rt.eja.revision().unwrap()).unwrap().is_empty());
        assert_eq!(
            state::concern_check_metas(&rt.eja)
                .get(&concern_id)
                .and_then(|m| m.release_witness_fact_id),
            Some(expected_watch_fact_id)
        );
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|ev| ev["event"] == "notify"
                && ev["body"]
                    .as_str()
                    .is_some_and(|s| s.contains("ready"))));

        // Tampering with watch_metric invalidates guard_tool scheduling immediately.
        fs::write(tool_dir.join("run.sh"), "echo tampered\n").unwrap();
        let bad_wake = rt
            .eja
            .act(
                rt.eja.revision().unwrap(),
                "wake",
                json!({
                    "delay_seconds": 1,
                    "reason": "tampered tool watch",
                    "guard_tool": "watch_metric",
                    "trigger_on": "changed"
                }),
            )
            .unwrap();
        let bad_out = rt.hands.execute(&bad_wake, &rt.eja, &rt.sandbox);
        assert!(matches!(bad_out, Outcome::Failed(ref msg) if msg.contains("校验和不匹配")));
        rt.eja.finish(bad_wake.id, bad_out).unwrap();
    }
}
