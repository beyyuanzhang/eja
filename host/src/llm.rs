use crate::config::{ProviderConfig, ProviderKind};
use crate::hands::ToolSpec;
use crate::mcp::truncate_utf8;
use eja_core::{Act, Concern, Eja, Entry, Evidence, Fact, Id, Keep, SearchHits, Step};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

pub const WINDOW_MAX_BYTES: usize = 256 * 1024;

pub struct TurnContext<'a> {
    pub provider: &'a ProviderConfig,
    pub model: &'a str,
    pub tools: &'a [ToolSpec],
    pub data_dir: &'a str,
    pub local_tz: &'a str,
    pub window: &'a [Value],
}

#[derive(Default)]
pub struct StepFeedback<'a> {
    pub entries: &'a [Entry],
    pub actions: &'a [Id],
    pub reads: &'a [Result<Evidence, String>],
    pub searches: &'a [Result<SearchHits, String>],
    pub commit_error: Option<&'a str>,
}

#[derive(Default)]
pub struct TurnSession {
    messages: Vec<Value>,
    pending: Vec<(String, PendingCall)>,
}

enum PendingCall {
    Keep,
    Release,
    Read,
    Search,
    Action,
    Invalid(String),
}

impl TurnSession {
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Decision {
    pub step: Step,
    pub reply: Option<String>,
}

pub trait Brain: Send + Sync {
    fn judge(
        &self,
        ctx: &TurnContext<'_>,
        turn: &mut TurnSession,
        feedback: StepFeedback<'_>,
    ) -> Result<Decision, String>;

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String>;
}

pub struct HttpBrain;

impl Brain for HttpBrain {
    fn judge(
        &self,
        ctx: &TurnContext<'_>,
        turn: &mut TurnSession,
        feedback: StepFeedback<'_>,
    ) -> Result<Decision, String> {
        let defs = build_tool_defs(ctx.tools);
        let tool_map: HashMap<&str, &str> = ctx
            .tools
            .iter()
            .map(|spec| (spec.name.as_str(), spec.capability.as_str()))
            .collect();
        let system = format!(
            "你是 EJA，一个持续存在的自我同一性主体。\n\
            数据目录：{}\n\
            配置文件：{}/config.json（`connectors` 变更会自动热加载）\n\
            本地时区：{}\n\n\
            前置消息是你按时间正序排列的不可变主体日志；末尾新增的 `noticed` 记录是本次触发。不调工具时的文本回复会发送给人，留空则保持沉默。",
            ctx.data_dir, ctx.data_dir, ctx.local_tz
        );

        if turn.messages.is_empty() {
            turn.messages.extend_from_slice(ctx.window);
        } else {
            turn.append_feedback(&ctx.provider.kind, feedback);
        }

        let completion = complete(
            ctx.provider,
            ctx.model,
            &system,
            &turn.messages,
            Some(&defs),
        )?;

        let mut step = Step::default();
        for ToolInvocation { id, name, args } in completion.calls {
            let kind = match name.as_str() {
                "keep" => {
                    let text = args
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() {
                        step.keep.push(Keep {
                            concern: args.get("concern").and_then(Value::as_i64),
                            text: text.to_string(),
                        });
                        PendingCall::Keep
                    } else {
                        PendingCall::Invalid("keep 需要非空 text".into())
                    }
                }
                "release" => match args.get("concern").and_then(Value::as_i64) {
                    Some(id) => {
                        step.release.push(id);
                        PendingCall::Release
                    }
                    None => PendingCall::Invalid("release 需要整数 concern".into()),
                },
                "read" => match args.get("fact").and_then(Value::as_i64) {
                    Some(id) => {
                        step.read.push(id);
                        PendingCall::Read
                    }
                    None => PendingCall::Invalid("read 需要整数 fact".into()),
                },
                "search" => {
                    let cue = args.get("cue").and_then(Value::as_str).unwrap_or("").trim();
                    if !cue.is_empty() {
                        step.search.push(cue.to_string());
                        PendingCall::Search
                    } else {
                        PendingCall::Invalid("search 需要非空 cue".into())
                    }
                }
                other => {
                    if let Some(&capability) = tool_map.get(other) {
                        step.act.push(Act {
                            capability: capability.to_string(),
                            input: args,
                        });
                        PendingCall::Action
                    } else {
                        PendingCall::Invalid(format!("未知工具: {other}"))
                    }
                }
            };
            turn.pending.push((id, kind));
        }

        let reply = if turn.pending.is_empty() {
            let trimmed = completion.text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };

        turn.messages.push(completion.message);
        Ok(Decision { step, reply })
    }

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String> {
        let text = complete(
            provider,
            model,
            "Reply with OK.",
            &[user_message("ping")],
            None,
        )?
        .text;
        Ok(format!(
            "连接成功 ({model}): {}",
            truncate_utf8(text.trim(), 60)
        ))
    }
}

impl TurnSession {
    fn append_feedback(&mut self, provider: &ProviderKind, feedback: StepFeedback<'_>) {
        let mut kept = feedback
            .entries
            .iter()
            .filter(|entry| matches!(entry.fact, Fact::Kept { .. }));
        let mut released = feedback
            .entries
            .iter()
            .filter(|entry| matches!(entry.fact, Fact::Released { .. }));
        let mut reads = feedback.reads.iter();
        let mut searches = feedback.searches.iter();
        let mut actions = feedback.actions.iter().copied();
        let receipts: HashMap<_, _> = feedback
            .entries
            .iter()
            .filter_map(|entry| {
                if let Fact::Returned { action, outcome } = &entry.fact {
                    Some((*action, (entry.id, outcome)))
                } else {
                    None
                }
            })
            .collect();
        let mut sent: HashSet<_> = feedback.actions.iter().copied().collect();
        let mut blocks = Vec::new();

        for (id, kind) in self.pending.drain(..) {
            let data = match kind {
                PendingCall::Keep | PendingCall::Release => {
                    if let Some(err) = feedback.commit_error {
                        Some(json!({ "error": err }))
                    } else {
                        let entry = match kind {
                            PendingCall::Keep => kept.next(),
                            _ => released.next(),
                        };
                        entry.map(|entry| {
                            sent.insert(entry.id);
                            match entry.fact {
                                Fact::Kept { concern, .. } => {
                                    json!({ "status": "kept", "concern": concern, "fact_id": entry.id })
                                }
                                Fact::Released { concern } => {
                                    json!({ "status": "released", "concern": concern, "fact_id": entry.id })
                                }
                                _ => unreachable!(),
                            }
                        })
                    }
                }
                PendingCall::Read => reads.next().map(|result| match result {
                    Ok(evidence) => json!(evidence),
                    Err(err) => json!({ "error": err }),
                }),
                PendingCall::Search => searches.next().map(|result| match result {
                    Ok(hits) => json!(hits),
                    Err(err) => json!({ "error": err }),
                }),
                PendingCall::Action => {
                    if let Some(err) = feedback.commit_error {
                        Some(json!({ "error": err }))
                    } else {
                        actions.next().and_then(|action| {
                            receipts.get(&action).map(|(id, outcome)| {
                                sent.insert(*id);
                                json!({ "id": id, "action": action, "outcome": outcome })
                            })
                        })
                    }
                }
                PendingCall::Invalid(reason) => Some(json!({ "error": reason })),
            }
            .unwrap_or_else(|| json!({ "error": "工具结果缺失" }))
            .to_string();
            match provider {
                ProviderKind::OpenAi => self.messages.push(json!({
                    "role": "tool", "tool_call_id": id, "content": data
                })),
                ProviderKind::Anthropic => blocks.push(json!({
                    "type": "tool_result", "tool_use_id": id, "content": data
                })),
            }
        }

        let extra_lines: Vec<String> = feedback
            .entries
            .iter()
            .filter(|entry| !sent.contains(&entry.id))
            .filter_map(|entry| serde_json::to_string(entry).ok())
            .collect();
        if !extra_lines.is_empty() {
            let extra = format!("{}\n", extra_lines.join("\n"));
            match provider {
                ProviderKind::OpenAi => self.messages.push(user_message(extra)),
                ProviderKind::Anthropic => {
                    blocks.push(json!({ "type": "text", "text": extra }));
                }
            }
        }
        if !blocks.is_empty() {
            self.messages
                .push(json!({ "role": "user", "content": blocks }));
        }
    }
}

/// Project the immutable Core journal from `anchor` through `head` into a strictly append-only
/// sequence of immutable JSON record messages.
/// Advances `anchor` only when the projected window exceeds `max_bytes`.
pub fn project_window(
    eja: &Eja,
    anchor: &mut Id,
    max_bytes: usize,
) -> eja_core::Result<Vec<Value>> {
    let head = eja.view()?.revision;
    if head == 0 {
        *anchor = 0;
        return Ok(Vec::new());
    }
    if *anchor > head {
        *anchor = 0;
    }
    let rendered = render_window(eja, *anchor, head)?;
    if window_bytes(&rendered) <= max_bytes {
        return Ok(rendered);
    }
    let entries = entries_since(eja, *anchor, head)?;
    let target_bytes = max_bytes / 2;
    let mut kept_bytes = 0usize;
    let mut keep_from = entries.len().saturating_sub(1);
    for idx in (0..entries.len()).rev() {
        let line_len = serde_json::to_string(&entries[idx])?.len() + 1;
        if idx < entries.len() - 1 && kept_bytes + line_len > target_bytes {
            break;
        }
        kept_bytes += line_len;
        keep_from = idx;
    }
    let new_anchor = if keep_from > 0 {
        entries[keep_from - 1].id
    } else {
        *anchor
    };
    if new_anchor > *anchor {
        *anchor = new_anchor;
        render_window(eja, *anchor, head)
    } else {
        Ok(rendered)
    }
}

pub fn window_bytes(records: &[Value]) -> usize {
    records
        .iter()
        .map(|msg| msg["content"].as_str().map_or(0, str::len) + 1)
        .sum()
}

pub fn render_window(eja: &Eja, anchor: Id, head: Id) -> eja_core::Result<Vec<Value>> {
    let mut out = Vec::new();
    if anchor > 0 {
        let concerns = concerns_at(eja, anchor)?;
        out.push(user_message(serde_json::to_string(&json!({
            "anchor": anchor,
            "concerns": concerns
        }))?));
    }
    for entry in entries_since(eja, anchor, head)? {
        match &entry.fact {
            Fact::Said { text } => out.push(json!({ "role": "assistant", "content": text })),
            _ => out.push(user_message(serde_json::to_string(&entry)?)),
        }
    }
    Ok(out)
}

pub fn entries_since(eja: &Eja, after: Id, mut through: Id) -> eja_core::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    while through > after {
        let page = eja.history(through)?;
        let Some(last_id) = page.last().map(|entry| entry.id) else {
            break;
        };
        let reached = last_id <= after;
        entries.extend(page.into_iter().take_while(|entry| entry.id > after));
        if reached || last_id <= 1 {
            break;
        }
        through = last_id - 1;
    }
    entries.reverse();
    Ok(entries)
}

pub fn concerns_at(eja: &Eja, revision: Id) -> eja_core::Result<Vec<Concern>> {
    if revision == 0 {
        return Ok(Vec::new());
    }
    let mut all = Vec::new();
    let mut cursor = revision;
    loop {
        let page = eja.concerns(revision, cursor)?;
        let count = page.len();
        let Some(last_latest) = page.last().map(|item| item.latest) else {
            break;
        };
        all.extend(page);
        if count < eja_core::PAGE || last_latest <= 1 {
            break;
        }
        cursor = last_latest - 1;
    }
    Ok(all)
}

struct WireToolDef {
    name: String,
    description: String,
    schema: Value,
}

fn build_tool_defs(hands_tools: &[ToolSpec]) -> Vec<WireToolDef> {
    let mut defs = vec![
        WireToolDef {
            name: "keep".into(),
            description: "新建或修订一条跨窗口挂念。".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "concern": { "type": "integer", "description": "要更新的挂念 ID，新建则省略" },
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
        },
        WireToolDef {
            name: "release".into(),
            description: "放下不再需要的挂念。".into(),
            schema: json!({
                "type": "object",
                "properties": { "concern": { "type": "integer" } },
                "required": ["concern"]
            }),
        },
        WireToolDef {
            name: "read".into(),
            description: "按 ID 读取历史日志条目原文。".into(),
            schema: json!({
                "type": "object",
                "properties": { "fact": { "type": "integer" } },
                "required": ["fact"]
            }),
        },
        WireToolDef {
            name: "search".into(),
            description: "在完整历史日志中检索相关条目。".into(),
            schema: json!({
                "type": "object",
                "properties": { "cue": { "type": "string" } },
                "required": ["cue"]
            }),
        },
    ];
    for spec in hands_tools {
        defs.push(WireToolDef {
            name: spec.name.clone(),
            description: spec.description.clone(),
            schema: spec.schema.clone(),
        });
    }
    defs
}

struct ToolInvocation {
    id: String,
    name: String,
    args: Value,
}

struct Completion {
    message: Value,
    text: String,
    calls: Vec<ToolInvocation>,
}

fn user_message(content: impl Into<String>) -> Value {
    json!({ "role": "user", "content": content.into() })
}

fn complete(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[WireToolDef]>,
) -> Result<Completion, String> {
    match provider.kind {
        ProviderKind::OpenAi => call_openai(provider, model, system, messages, tools),
        ProviderKind::Anthropic => call_anthropic(provider, model, system, messages, tools),
    }
}

fn call_openai(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[WireToolDef]>,
) -> Result<Completion, String> {
    let base = provider.base_url.trim().trim_end_matches('/');
    let url = if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    };
    let system_message = json!({ "role": "system", "content": system });
    let messages: Vec<_> = std::iter::once(&system_message).chain(messages).collect();
    let mut body = json!({
        "model": model,
        "messages": messages
    });
    if let Some(list) = tools {
        let mapped: Vec<Value> = list
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.schema
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(mapped);
    }
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if !provider.api_key.trim().is_empty() {
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", provider.api_key.trim()),
        ));
    }
    let resp = post_json(&url, &headers, &body)?;
    let msg = resp.pointer("/choices/0/message").ok_or_else(|| {
        format!(
            "LLM 响应缺少 choices[0].message: {}",
            truncate_utf8(&resp.to_string(), 240)
        )
    })?;
    let text = msg
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut message = json!({ "role": "assistant", "content": msg["content"] });
    if let Some(reasoning) = msg
        .get("reasoning_content")
        .filter(|value| value.is_string())
    {
        message["reasoning_content"] = reasoning.clone();
    }
    let mut calls = Vec::new();
    if let Some(arr) = msg.get("tool_calls").and_then(Value::as_array) {
        let mut wire_calls = Vec::new();
        for (index, item) in arr.iter().enumerate() {
            let Some(name) = item.pointer("/function/name").and_then(Value::as_str) else {
                continue;
            };
            let id = item["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{}_{index}", messages.len()));
            let (args, arguments) = match item.pointer("/function/arguments") {
                Some(Value::String(s)) => (
                    serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                    s.clone(),
                ),
                Some(v) if v.is_object() => (v.clone(), v.to_string()),
                _ => (json!({}), "{}".into()),
            };
            wire_calls.push(json!({
                "id": id, "type": "function",
                "function": { "name": name, "arguments": arguments }
            }));
            calls.push(ToolInvocation {
                id,
                name: name.into(),
                args,
            });
        }
        if !wire_calls.is_empty() {
            message["tool_calls"] = json!(wire_calls);
        }
    }
    Ok(Completion {
        message,
        text,
        calls,
    })
}

fn call_anthropic(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[WireToolDef]>,
) -> Result<Completion, String> {
    let base = provider.base_url.trim().trim_end_matches('/');
    let url = if base.ends_with("/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };
    let mut body = json!({
        "model": model,
        "max_tokens": 16384,
        "system": system,
        "messages": messages
    });
    if let Some(list) = tools {
        let mapped: Vec<Value> = list
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "input_schema": spec.schema
                })
            })
            .collect();
        body["tools"] = Value::Array(mapped);
    }
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("x-api-key".to_string(), provider.api_key.trim().to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
    ];
    let resp = post_json(&url, &headers, &body)?;
    let blocks = resp
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "Anthropic 响应缺少 content: {}",
                truncate_utf8(&resp.to_string(), 240)
            )
        })?;
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    texts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                    calls.push(ToolInvocation {
                        id: id.into(),
                        name: name.into(),
                        args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(Completion {
        message: json!({ "role": "assistant", "content": blocks }),
        text: texts.join("\n"),
        calls,
    })
}

fn post_json(url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|err| err.to_string())?;
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args([
        "-sS",
        "-X",
        "POST",
        "--max-time",
        "300",
        "-w",
        "\n%{http_code}",
    ]);
    for (k, v) in headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    cmd.args(["--data-binary", "@-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("启动 curl 失败: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&bytes)
            .map_err(|err| format!("写入请求体失败: {err}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("等待 LLM 响应失败: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "网络请求失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let (resp_body, code_str) = raw.rsplit_once('\n').unwrap_or((&raw, "0"));
    let status: u16 = code_str.trim().parse().unwrap_or(0);
    let parsed: Value =
        serde_json::from_str(resp_body).unwrap_or_else(|_| json!({ "raw": resp_body }));
    if !(200..300).contains(&status) {
        let detail = parsed
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or(resp_body);
        return Err(format!("HTTP {status}: {}", truncate_utf8(detail, 280)));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eja_core::{Outcome, Source};

    #[test]
    fn deterministic_window_projection_is_strictly_append_only_until_reset() -> eja_core::Result<()>
    {
        let eja = Eja::open(":memory:")?;
        let mut anchor = 0;
        let first = eja.notice(Source::Person, "first notice")?;
        let w1 = project_window(&eja, &mut anchor, 4096)?;
        assert_eq!(anchor, 0);

        eja.commit(
            first,
            &Step {
                keep: vec![Keep {
                    concern: None,
                    text: "track workshop labels".into(),
                }],
                ..Step::default()
            },
            &[],
        )?;
        eja.say("noted")?;
        eja.notice(Source::Person, "second notice")?;
        let w2 = project_window(&eja, &mut anchor, 4096)?;
        assert_eq!(anchor, 0);
        assert!(w2.starts_with(&w1));
        assert!(w2
            .iter()
            .any(|m| m["role"] == "assistant" && m["content"] == "noted"));

        let w3 = project_window(&eja, &mut anchor, 220)?;
        assert!(anchor > 0);
        let header: Value = serde_json::from_str(w3[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(header["anchor"], anchor);
        assert_eq!(header["concerns"][0]["text"], "track workshop labels");
        Ok(())
    }

    #[test]
    fn batched_tool_results_follow_call_ids_in_both_protocols() -> eja_core::Result<()> {
        let eja = Eja::open(":memory:")?;
        let input = eja.notice(Source::Person, "check")?;
        let calls = eja.commit(
            input,
            &Step {
                keep: vec![Keep {
                    concern: None,
                    text: "follow up".into(),
                }],
                act: vec![Act {
                    capability: "check".into(),
                    input: Value::Null,
                }],
                ..Step::default()
            },
            &["check"],
        )?;
        let action = calls[0].id;
        let receipt = eja.finish(action, Outcome::Failed("unavailable".into()))?;
        let view = eja.view()?;
        let reads = vec![Ok(eja.evidence(input, view.revision)?)];
        let entries: Vec<_> = (input + 1..=receipt)
            .map(|id| eja.read(id))
            .collect::<eja_core::Result<_>>()?;

        for provider in [ProviderKind::OpenAi, ProviderKind::Anthropic] {
            let mut turn = TurnSession {
                messages: vec![user_message("start")],
                pending: vec![
                    ("action".into(), PendingCall::Action),
                    ("read".into(), PendingCall::Read),
                    ("keep".into(), PendingCall::Keep),
                ],
            };
            turn.append_feedback(
                &provider,
                StepFeedback {
                    entries: &entries,
                    actions: &[action],
                    reads: &reads,
                    searches: &[],
                    commit_error: None,
                },
            );
            let (results, id_key) = match provider {
                ProviderKind::OpenAi => (&turn.messages[1..], "tool_call_id"),
                ProviderKind::Anthropic => (
                    turn.messages[1]["content"].as_array().unwrap().as_slice(),
                    "tool_use_id",
                ),
            };
            assert_eq!(results.len(), 3);
            assert_eq!(results[0][id_key], "action");
            assert_eq!(results[1][id_key], "read");
            assert_eq!(results[2][id_key], "keep");
            let result = |index: usize| {
                serde_json::from_str::<Value>(results[index]["content"].as_str().unwrap()).unwrap()
            };
            assert_eq!(result(0)["id"], receipt);
            assert_eq!(result(0)["outcome"]["status"], "failed");
            assert_eq!(result(1)["id"], input);
            assert_eq!(result(2)["status"], "kept");
            assert_eq!(result(2)["fact_id"], input + 1);
            assert_eq!(result(2)["concern"], input + 1);
        }
        Ok(())
    }
}
