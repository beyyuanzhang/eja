use crate::config::CapabilityConfig;
use crate::mcp::{CapabilityPool, CapabilityStatus};
use crate::oauth::OAuthToken;
use crate::sandbox::{Sandbox, TrustedDir};
use crate::state::watch::{self, WakeAlarm, WakeGuard};
use crate::tool::{ToolExecution, ToolSpec, ToolTarget};
use eja_core::{eja, Call, Outcome};
use serde_json::{json, Value};
use std::collections::HashSet;

pub const ANSWER_WITH_RECEIPT: &str = "answer_with_receipt";

fn builtin_spec(name: &str, description: &str, schema: Value, target: ToolTarget) -> ToolSpec {
    ToolSpec {
        capability: name.into(),
        name: name.into(),
        description: description.into(),
        schema,
        target,
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
    capabilities: CapabilityPool,
}

impl Hands {
    pub fn open(
        data_dir: TrustedDir,
        core_dir: TrustedDir,
        sandbox: Sandbox,
        capabilities: &[CapabilityConfig],
    ) -> Self {
        let mut pool = CapabilityPool::with_core_dir(sandbox, core_dir);
        pool.register_local(capabilities);
        Self {
            data_dir,
            capabilities: pool,
        }
    }

    pub fn reload(&mut self, capabilities: &[CapabilityConfig], force: bool) {
        self.capabilities.reload(capabilities, force);
    }

    pub fn disconnect_dead(&mut self) -> bool {
        self.capabilities.disconnect_dead()
    }

    pub fn is_current(&self, spec: &ToolSpec) -> bool {
        match spec.target {
            ToolTarget::Capability(generation) => {
                self.capabilities.is_current(&spec.capability, generation)
            }
            _ => true,
        }
    }

    pub fn set_oauth_code(&mut self, id: &str, code: Option<String>) {
        self.capabilities.set_oauth_code(id, code);
    }

    pub fn save_oauth_token(
        &mut self,
        id: &str,
        token: OAuthToken,
        capabilities: &[CapabilityConfig],
    ) -> Result<(), String> {
        self.capabilities.save_oauth_token(id, token)?;
        self.capabilities.reload(capabilities, false);
        Ok(())
    }

    pub fn capability_statuses(&self) -> Vec<CapabilityStatus> {
        self.capabilities.statuses()
    }

    pub fn tools(&self, receipt_answer: bool) -> Vec<ToolSpec> {
        let mut specs = vec![
            builtin_spec(
                "keep",
                "新建或修订挂念（旧日志滑出上下文窗口后，仅活跃挂念与未决未知常驻上下文；跨回合约定、长期偏好与待办须记入挂念），可绑定已注册的验收规则。",
                json!({
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer" },
                        "text": { "type": "string" },
                        "checker": { "type": "string" }
                    },
                    "required": ["text"]
                }),
                ToolTarget::Kernel,
            ),
            builtin_spec(
                "release",
                "放下挂念。绑定规则的挂念需要该规则的新命中记录 ID。如需结束另行预约的守望，请显式调用 wake 取消。",
                json!({
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer" },
                        "witness_fact_id": { "type": "integer", "description": "验证该挂念已完成/可放下的客观事实 id" }
                    },
                    "required": ["concern"]
                }),
                ToolTarget::Kernel,
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
                ToolTarget::Kernel,
            ),
            builtin_spec(
                "search",
                "从全历史主体日志中检索相关旧事实。",
                json!({
                    "type": "object",
                    "properties": { "cue": { "type": "string" } },
                    "required": ["cue"]
                }),
                ToolTarget::Kernel,
            ),
            builtin_spec(
                "wake",
                "预约一次唤醒，或静默轮询已注册的本地检查规则直到命中。相同 reason 或相同 guard_tool 会覆盖旧预约；delay_seconds 为 0 时取消匹配预约。",
                json!({
                    "type": "object",
                    "properties": {
                        "delay_seconds": { "type": "integer", "minimum": 0, "description": "正整数为延迟/轮询秒数；0 表示取消匹配 reason 或 guard_tool 的预约" },
                        "reason": { "type": "string" },
                        "guard_tool": { "type": "string", "description": "可选：规则名称。未命中则按 delay_seconds 间隔复查。" },
                        "max_checks": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 60 }
                    },
                    "required": ["delay_seconds", "reason"]
                }),
                ToolTarget::Host,
            ),
            builtin_spec(
                "promote_tool",
                "注册可复用的本地文件文本检查（支持 equals、contains、not_equals，至少提供一项）。名称固定，同一规则可用于静默守望（guard_tool）和挂念验收（checker）。",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "file": { "type": "string", "description": "eja 数据目录中的单个文件名" },
                        "equals": { "type": "string", "description": "可选：忽略文件末尾换行后须等于此文本" },
                        "contains": { "type": "string", "description": "可选：文件内容须包含此非空子串" },
                        "not_equals": { "type": "string", "description": "可选：忽略文件末尾换行后须不等于此文本（例如空串 \"\" 表示非空文件）" }
                    },
                    "required": ["name", "file"],
                    "additionalProperties": false
                }),
                ToolTarget::Host,
            ),
        ];

        if receipt_answer {
            specs.push(builtin_spec(
                ANSWER_WITH_RECEIPT,
                "用检索已返回的已观测回执结束回合。此调用须单独提交。",
                json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "minLength": 1 },
                        "receipt": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["text", "receipt"],
                    "additionalProperties": false
                }),
                ToolTarget::Kernel,
            ));
        }
        let mut used: HashSet<String> = specs.iter().map(|spec| spec.name.clone()).collect();

        for (tool, generation) in self.capabilities.tools() {
            let name = safe_tool_name(&tool.capability, &mut used);
            let description = if tool.description.is_empty() {
                format!("能力工具 {}", tool.capability)
            } else {
                tool.description.clone()
            };
            specs.push(ToolSpec {
                capability: tool.capability.clone(),
                name,
                description,
                schema: tool.input_schema.clone(),
                target: ToolTarget::Capability(generation),
            });
        }
        specs
    }

    pub fn execute(&mut self, call: &Call, eja: &eja) -> ToolExecution {
        match call.capability.as_str() {
            "wake" => execute_wake(eja, call.id, &call.input).into(),
            "promote_tool" => {
                let (Some(name), Some(file)) =
                    (call.input["name"].as_str(), call.input["file"].as_str())
                else {
                    return Outcome::Failed(
                        "需要 name、file，以及 equals/contains/not_equals 中至少一项".into(),
                    )
                    .into();
                };
                let parse_opt = |key: &str| match call.input.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.as_str())),
                    Some(_) => Err(format!("{key} 须为字符串")),
                };
                let (equals, contains, not_equals) = match (
                    parse_opt("equals"),
                    parse_opt("contains"),
                    parse_opt("not_equals"),
                ) {
                    (Ok(eq), Ok(ct), Ok(ne)) => (eq, ct, ne),
                    (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                        return Outcome::Failed(e).into();
                    }
                };
                match watch::register_check(eja, name, file, equals, contains, not_equals) {
                    Ok(()) => Outcome::Observed(format!("已注册检查规则 {name}")),
                    Err(err) => Outcome::Failed(err.to_string()),
                }
                .into()
            }
            other => self
                .capabilities
                .call(other, call.id, &call.input, &self.data_dir),
        }
    }
}

fn execute_wake(eja: &eja, call_id: eja_core::Id, input: &Value) -> Outcome {
    let result = (|| {
        let invalid = |message: &str| eja_core::Error::Invalid(message.into());
        let delay = input["delay_seconds"]
            .as_i64()
            .filter(|delay| *delay >= 0)
            .ok_or_else(|| invalid("wake 需要非负整数 delay_seconds（0 表示取消匹配预约）"))?;
        let reason = input["reason"]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| invalid("wake 需要非空 reason"))?;
        if input.as_object().is_some_and(|args| {
            args.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "delay_seconds" | "reason" | "guard_tool" | "max_checks"
                )
            })
        }) {
            return Err(invalid(
                "wake 仅接受 delay_seconds、reason、guard_tool 与 max_checks",
            ));
        }
        let guard_name = match input.get("guard_tool") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| invalid("guard_tool 需要规则名称"))?,
            ),
        };
        if delay == 0 {
            let removed = watch::cancel_wakes(eja, reason, guard_name)?;
            return Ok(format!("cancelled {removed} wake(s) ({reason})"));
        }
        let guard = if let Some(checker) = guard_name {
            watch::registered_check(eja, checker)?;
            let remaining = match input.get("max_checks") {
                None => 60,
                Some(value) => value
                    .as_u64()
                    .filter(|count| (1..=1000).contains(count))
                    .ok_or_else(|| invalid("max_checks 须为 1–1000 的整数"))?
                    as u32,
            };
            Some(WakeGuard {
                checker: checker.into(),
                interval_seconds: delay,
                remaining,
            })
        } else {
            if input.get("max_checks").is_some() {
                return Err(invalid("max_checks 需要 guard_tool"));
            }
            None
        };
        let fire_at = eja
            .now()?
            .as_unix()
            .checked_add(delay)
            .ok_or_else(|| invalid("唤醒时间超出范围"))?;
        let display = format_local_unix(fire_at);
        watch::schedule_wakes(
            eja,
            &[WakeAlarm {
                id: format!("w-{fire_at}-{call_id}"),
                fire_at_unix: fire_at,
                fire_at_display: display.clone(),
                reason: reason.into(),
                guard,
            }],
        )?;
        Ok(format!("scheduled at {display} ({reason})"))
    })();
    match result {
        Ok(detail) => Outcome::Observed(detail),
        Err(err) => Outcome::Failed(err.to_string()),
    }
}

fn safe_tool_name(capability: &str, used: &mut HashSet<String>) -> String {
    let base: String = capability
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
    let mut candidate = base.clone();
    let mut idx = 2usize;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}_{idx}");
        idx += 1;
    }
    candidate
}

pub fn format_local_unix(secs: i64) -> String {
    let time = secs as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    let ptr = unsafe { libc::localtime_r(&time, tm.as_mut_ptr()) };
    if ptr.is_null() {
        return secs.to_string();
    }
    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_local_unix_produces_iso_like_timestamp() {
        let formatted = format_local_unix(1_700_000_000);
        assert_eq!(formatted.len(), 19);
        assert_eq!(&formatted[4..5], "-");
        assert_eq!(&formatted[7..8], "-");
        assert_eq!(&formatted[10..11], " ");
        assert_eq!(&formatted[13..14], ":");
        assert_eq!(&formatted[16..17], ":");
    }
}
