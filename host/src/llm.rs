use crate::config::{ProviderConfig, ProviderKind};
use crate::hands::ToolSpec;
use crate::mcp::truncate_utf8;
use eja_core::{Eja, Entry, Evidence, Fact, Id};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;

pub const WINDOW_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
pub struct ModelReply {
    pub message: Value,
    pub text: String,
    pub calls: Vec<ToolCall>,
}

pub trait Brain: Send + Sync {
    fn complete(
        &self,
        provider: &ProviderConfig,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<ModelReply, String>;

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String>;
}

pub struct HttpBrain;

impl Brain for HttpBrain {
    fn complete(
        &self,
        provider: &ProviderConfig,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<ModelReply, String> {
        let defs = build_tool_defs(tools);
        complete_wire(provider, model, system, messages, Some(&defs))
    }

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String> {
        let text = complete_wire(
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

pub enum ToolOutput {
    Recorded(Value),
    Temporary(Value),
    Read(Evidence, usize),
}

pub enum ToolMessages {
    Exchange(Vec<Value>),
    Notes(Vec<Value>),
}

pub fn tool_messages(
    provider: &ProviderKind,
    reply: ModelReply,
    window_bytes: usize,
    mut execute: impl FnMut(&ToolCall) -> Result<ToolOutput, String>,
) -> Result<ToolMessages, String> {
    let count = reply.calls.len();
    let blocks: Vec<_> = reply
        .calls
        .iter()
        .map(|call| match provider {
            ProviderKind::OpenAi => json!({"role":"tool", "tool_call_id":call.id, "content":""}),
            ProviderKind::Anthropic => {
                json!({"type":"tool_result", "tool_use_id":call.id, "content":""})
            }
        })
        .collect();
    let mut tail = match provider {
        ProviderKind::OpenAi => blocks,
        ProviderKind::Anthropic => vec![json!({"role":"user", "content":blocks})],
    };
    let framing = messages_bytes(&tail);
    let reserve = (window_bytes / 4).min(64 * 1024);
    let per_call =
        window_bytes.saturating_sub(reserve + msg_bytes(&reply.message) + framing) / count;
    let exchange = per_call >= 128;
    let slots = match provider {
        ProviderKind::OpenAi => tail.as_mut_slice(),
        ProviderKind::Anthropic => tail[0]["content"]
            .as_array_mut()
            .expect("tool result blocks")
            .as_mut_slice(),
    };
    let mut notes = Vec::new();
    for (call, slot) in reply.calls.into_iter().zip(slots) {
        let output = execute(&call)?;
        let (kind, budget) = if exchange {
            (provider, per_call)
        } else {
            // Journal projection already includes durable effects. Keep transient results as text.
            let framing =
                json!({"tool": call.name, "result": null}).to_string().len() + 1 - "null".len();
            let available = (window_bytes / 2 / count).saturating_sub(framing);
            if matches!(output, ToolOutput::Recorded(_)) || available < "null".len() {
                continue;
            }
            (&ProviderKind::OpenAi, available)
        };
        let value = match output {
            ToolOutput::Read(evidence, offset) => {
                format_read_result(kind, &evidence, offset, budget)
            }
            ToolOutput::Recorded(v) | ToolOutput::Temporary(v) => cap_tool_result(kind, v, budget),
        };
        if exchange {
            slot["content"] = Value::String(value.to_string());
        } else {
            notes.push(user_message(
                json!({"tool": call.name, "result": value}).to_string(),
            ));
        }
    }
    if exchange {
        tail.insert(0, reply.message);
        Ok(ToolMessages::Exchange(tail))
    } else {
        Ok(ToolMessages::Notes(notes))
    }
}

impl ToolMessages {
    pub fn apply(
        self,
        messages: &mut Vec<Value>,
        eja: &Eja,
        anchor: &mut Id,
        before: Id,
        trigger: Id,
        max_bytes: usize,
    ) -> eja_core::Result<()> {
        let (head, tail) = match self {
            Self::Exchange(tail)
                if messages_bytes(messages) + messages_bytes(&tail) <= max_bytes =>
            {
                messages.extend(tail);
                return Ok(());
            }
            Self::Exchange(tail) => (before, tail),
            Self::Notes(tail) => (eja.revision()?, tail),
        };
        let budget = max_bytes.saturating_sub(messages_bytes(&tail));
        let mut prefix = project_window_through(eja, anchor, head, budget, trigger)?;
        prefix.extend(tail);
        *messages = prefix;
        Ok(())
    }
}

/// Project a turn at a fixed revision, keeping its triggering notice in the window.
pub fn project_window_through(
    eja: &Eja,
    anchor: &mut Id,
    through: Id,
    max_bytes: usize,
    trigger: Id,
) -> eja_core::Result<Vec<Value>> {
    if max_bytes == 0 {
        return Ok(Vec::new());
    }
    if *anchor > through {
        *anchor = 0;
    }
    *anchor = (*anchor).min(trigger.saturating_sub(1));
    let entries = eja.entries_for_turn(*anchor, through, trigger)?;
    let entry_msgs: Vec<Value> = entries
        .iter()
        .map(|entry| {
            // A queued trigger may precede the reply to an earlier turn. Supply
            // that reply as journal evidence, never as an assistant prefill.
            if entry.id > trigger && matches!(entry.fact, Fact::Said { .. }) {
                Ok(user_message(serde_json::to_string(entry)?))
            } else {
                render_entry(entry)
            }
        })
        .collect::<eja_core::Result<_>>()?;
    let header_budget = max_bytes / 2;
    let header = if *anchor > 0 {
        render_anchor_header(eja, *anchor, header_budget)?
    } else {
        None
    };
    let total_bytes =
        header.as_ref().map_or(0, msg_bytes) + entry_msgs.iter().map(msg_bytes).sum::<usize>();
    if total_bytes <= max_bytes {
        return Ok(header.into_iter().chain(entry_msgs).collect());
    }

    let target_bytes = max_bytes / 2;
    let mut kept_bytes = 0usize;
    let mut tail_start = entries.len().saturating_sub(1);
    for idx in (0..entries.len()).rev() {
        let line_len = msg_bytes(&entry_msgs[idx]);
        if idx < entries.len() - 1 && kept_bytes + line_len > target_bytes {
            break;
        }
        kept_bytes += line_len;
        tail_start = idx;
    }
    let trigger_idx = entries
        .binary_search_by_key(&trigger, |entry| entry.id)
        .map_err(|_| eja_core::Error::Invalid(format!("窗口缺少触发记录 {trigger}")))?;
    let keep_from = tail_start.min(trigger_idx);

    let active_header = if keep_from > 0 {
        *anchor = entries[keep_from - 1].id;
        render_anchor_header(eja, *anchor, header_budget)?
    } else {
        header
    };
    let header_bytes = active_header.as_ref().map_or(0, msg_bytes);
    let remaining_budget = max_bytes.saturating_sub(header_bytes);
    let slice_bytes: usize = entry_msgs[keep_from..].iter().map(msg_bytes).sum();

    let mut out: Vec<Value> = active_header.into_iter().collect();
    if slice_bytes <= remaining_budget {
        out.extend(entry_msgs.into_iter().skip(keep_from));
        return Ok(out);
    }
    let later = &entry_msgs[trigger_idx + 1..];
    let tail_reserve = later.last().map_or(0, msg_bytes).min(remaining_budget / 2);
    let notice = bound_entry_message(
        &entries[trigger_idx],
        entry_msgs[trigger_idx].clone(),
        remaining_budget - tail_reserve,
    );
    let tail_budget = remaining_budget - msg_bytes(&notice);
    out.push(notice);
    collect_tail_within(&entries[trigger_idx + 1..], later, tail_budget, &mut out);
    Ok(out)
}

fn collect_tail_within(entries: &[Entry], msgs: &[Value], mut budget: usize, out: &mut Vec<Value>) {
    let mut selected = Vec::new();
    for idx in (0..entries.len()).rev() {
        let line_len = msg_bytes(&msgs[idx]);
        if line_len <= budget {
            budget -= line_len;
            selected.push(msgs[idx].clone());
        } else if selected.is_empty() && budget >= 1 {
            selected.push(bound_entry_message(
                &entries[idx],
                msgs[idx].clone(),
                budget,
            ));
            break;
        } else {
            break;
        }
    }
    out.extend(selected.into_iter().rev());
}

pub fn messages_bytes(messages: &[Value]) -> usize {
    messages.iter().map(msg_bytes).sum()
}

fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn ceil_char_boundary(s: &str, min_bytes: usize) -> usize {
    let mut end = min_bytes.min(s.len());
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    end
}

fn fit_utf8_prefix_len(text: &str, max_prefix: usize, fits: impl Fn(&str) -> bool) -> usize {
    let hi_max = floor_char_boundary(text, max_prefix);
    if fits(&text[..hi_max]) {
        return hi_max;
    }
    let mut lo = 0usize;
    let mut hi = hi_max;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let mut cut = floor_char_boundary(text, mid);
        if cut <= lo {
            cut = ceil_char_boundary(text, lo + 1);
            if cut > hi {
                break;
            }
        }
        if fits(&text[..cut]) {
            lo = cut;
        } else {
            hi = floor_char_boundary(text, cut - 1);
        }
    }
    lo
}

// Anthropic nests the serialized result in a JSON string inside a content block.
fn result_fits(serialized: &str, provider: &ProviderKind, budget: usize) -> bool {
    serialized.len() <= budget
        && (*provider == ProviderKind::OpenAi
            || serde_json::to_string(serialized).unwrap().len() - 2 <= budget)
}

fn cap_tool_result(provider: &ProviderKind, value: Value, max_bytes: usize) -> Value {
    let serialized = value.to_string();
    if result_fits(&serialized, provider, max_bytes) {
        return value;
    }
    let total_bytes = serialized.len();
    let make = |preview: &str| {
        json!({
            "truncated": true,
            "total_bytes": total_bytes,
            "preview": preview
        })
    };
    if !result_fits(&make("").to_string(), provider, max_bytes) {
        let minimal = json!({ "truncated": true });
        return if result_fits(&minimal.to_string(), provider, max_bytes) {
            minimal
        } else {
            Value::Null
        };
    }
    let len = fit_utf8_prefix_len(&serialized, max_bytes, |slice| {
        result_fits(&make(slice).to_string(), provider, max_bytes)
    });
    make(&serialized[..len])
}

fn format_read_result(
    provider: &ProviderKind,
    evidence: &Evidence,
    offset: usize,
    max_bytes: usize,
) -> Value {
    let fact_id = evidence.entry.id;
    let raw = match serde_json::to_string(evidence) {
        Ok(s) => s,
        Err(err) => {
            return cap_tool_result(provider, json!({ "error": err.to_string() }), max_bytes)
        }
    };
    if offset == 0 && result_fits(&raw, provider, max_bytes) {
        return json!(evidence);
    }
    let start = floor_char_boundary(&raw, offset.min(raw.len()));
    let remaining = &raw[start..];
    let total_bytes = raw.len();
    let build_page = |chunk: &str| {
        let end = start + chunk.len();
        let mut page = json!({
            "fact": fact_id, "offset": start, "total_bytes": total_bytes,
            "truncated": end < total_bytes, "chunk": chunk
        });
        if end < total_bytes {
            page["next_offset"] = json!(end);
        }
        page
    };
    if !result_fits(&build_page("").to_string(), provider, max_bytes) {
        return cap_tool_result(
            provider,
            json!({ "fact": fact_id, "offset": start, "total_bytes": total_bytes, "truncated": true }),
            max_bytes,
        );
    }
    let len = fit_utf8_prefix_len(remaining, max_bytes, |slice| {
        result_fits(&build_page(slice).to_string(), provider, max_bytes)
    });
    if len == 0 && !remaining.is_empty() {
        return cap_tool_result(
            provider,
            json!({"error": "窗口不足以容纳分页数据"}),
            max_bytes,
        );
    }
    build_page(&remaining[..len])
}

pub fn msg_bytes(msg: &Value) -> usize {
    if msg.get("tool_calls").is_some() {
        return msg.to_string().len() + 1;
    }
    msg["content"]
        .as_str()
        .map(|text| text.len() + 1)
        .unwrap_or_else(|| msg.to_string().len() + 1)
}

fn bound_entry_message(entry: &Entry, message: Value, budget: usize) -> Value {
    if msg_bytes(&message) <= budget {
        return message;
    }
    let content_budget = budget.saturating_sub(1);
    let role = message["role"].as_str().unwrap_or("user");
    let raw = message["content"].as_str().unwrap_or("");
    let suffix = format!(" [truncated; read fact {}]", entry.id);
    let bounded = if content_budget <= suffix.len() {
        truncate_utf8(&suffix, content_budget)
    } else {
        format!(
            "{}{suffix}",
            truncate_utf8(raw, content_budget - suffix.len())
        )
    };
    json!({ "role": role, "content": bounded })
}

fn render_anchor_header(eja: &Eja, anchor: Id, budget: usize) -> eja_core::Result<Option<Value>> {
    let content_budget = budget.saturating_sub(1);
    if content_budget == 0 {
        return Ok(None);
    }
    let concerns = eja.concerns(anchor)?;
    let total_concerns = concerns.len();
    let prefix = format!("{{\"anchor\":{anchor},\"concerns\":[");
    let base_len = prefix.len() + 2;
    if base_len > content_budget {
        return Ok(None);
    }
    let est_min_bytes: usize = base_len
        + concerns
            .iter()
            .map(|c| c.text.len().saturating_add(36))
            .sum::<usize>();
    if est_min_bytes <= content_budget {
        let full = serde_json::to_string(&json!({
            "anchor": anchor,
            "concerns": concerns
        }))?;
        if full.len() <= content_budget {
            return Ok(Some(user_message(full)));
        }
    }

    let omitted_len = |omitted: usize| {
        if omitted == 0 {
            0
        } else {
            format!(",\"omitted_concerns\":{omitted}").len()
        }
    };
    let max_text_each = (content_budget / total_concerns.max(1)).clamp(48, 2048);
    let mut packed: Vec<String> = Vec::new();
    let mut items_bytes = 0usize;
    for (index, concern) in concerns.iter().enumerate() {
        let remaining_after = total_concerns - index - 1;
        let comma = usize::from(!packed.is_empty());
        let overhead = base_len + items_bytes + comma + omitted_len(remaining_after);
        if overhead >= content_budget {
            break;
        }
        let item_budget = content_budget - overhead;
        if concern.text.len() <= max_text_each {
            let full_item = serde_json::to_string(concern)?;
            if full_item.len() <= item_budget {
                items_bytes += comma + full_item.len();
                packed.push(full_item);
                continue;
            }
        }
        let make_truncated = |text: &str| {
            serde_json::to_string(&json!({
                "id": concern.id,
                "latest": concern.latest,
                "at": concern.at,
                "text": text,
                "truncated": true
            }))
            .unwrap_or_default()
        };
        if make_truncated("").len() > item_budget {
            break;
        }
        let limit = fit_utf8_prefix_len(&concern.text, max_text_each, |slice| {
            make_truncated(slice).len() <= item_budget
        });
        let item_json = make_truncated(&concern.text[..limit]);
        items_bytes += comma + item_json.len();
        packed.push(item_json);
    }
    let omitted = total_concerns - packed.len();
    let omitted_suffix = if omitted > 0 {
        format!(",\"omitted_concerns\":{omitted}")
    } else {
        String::new()
    };
    let mut out = String::with_capacity(base_len + items_bytes + omitted_suffix.len());
    out.push_str(&prefix);
    for (index, item) in packed.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(item);
    }
    out.push(']');
    if !omitted_suffix.is_empty() && out.len() + omitted_suffix.len() < content_budget {
        out.push_str(&omitted_suffix);
    }
    out.push('}');
    Ok(Some(user_message(out)))
}

fn render_entry(entry: &Entry) -> eja_core::Result<Value> {
    match &entry.fact {
        Fact::Said { text, .. } => Ok(json!({ "role": "assistant", "content": text })),
        _ => Ok(user_message(serde_json::to_string(entry)?)),
    }
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
                "properties": {
                    "fact": { "type": "integer" },
                    "offset": { "type": "integer", "minimum": 0, "description": "可选，按字节偏移续读超长事实" }
                },
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

fn user_message(content: impl Into<String>) -> Value {
    json!({ "role": "user", "content": content.into() })
}

fn complete_wire(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[WireToolDef]>,
) -> Result<ModelReply, String> {
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
) -> Result<ModelReply, String> {
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
            calls.push(ToolCall {
                id,
                name: name.into(),
                args,
            });
        }
        if !wire_calls.is_empty() {
            message["tool_calls"] = json!(wire_calls);
        }
    }
    Ok(ModelReply {
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
) -> Result<ModelReply, String> {
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
                    calls.push(ToolCall {
                        id: id.into(),
                        name: name.into(),
                        args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(ModelReply {
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
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
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
    use eja_core::Source;

    #[test]
    fn deterministic_window_projection_is_strictly_append_only_until_reset() -> eja_core::Result<()>
    {
        let eja = Eja::open(":memory:")?;
        let mut anchor = 0;
        let first = eja.notice(Source::Person, "first notice")?;
        let w1 = project_window_through(&eja, &mut anchor, first, 4096, first)?;
        assert_eq!(anchor, 0);

        eja.keep(None, "track workshop labels")?;
        eja.say("noted")?;
        let second = eja.notice(Source::Person, "second notice")?;
        let w2 = project_window_through(&eja, &mut anchor, second, 4096, second)?;
        assert_eq!(anchor, 0);
        assert!(w2.starts_with(&w1));
        assert!(w2
            .iter()
            .any(|m| m["role"] == "assistant" && m["content"] == "noted"));

        let w3 = project_window_through(&eja, &mut anchor, second, 220, second)?;
        assert!(anchor > 0);
        let header: Value = serde_json::from_str(w3[0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(header["anchor"], anchor);
        assert_eq!(header["concerns"][0]["text"], "track workshop labels");
        Ok(())
    }

    #[test]
    fn tool_messages_preserve_order_and_ids_in_both_protocols() {
        for provider in [ProviderKind::OpenAi, ProviderKind::Anthropic] {
            let calls = [
                ("action", json!({"id":4,"outcome":{"status":"failed"}})),
                ("read", json!({"id":1})),
                ("keep", json!({"status":"kept","concern":2,"fact_id":2})),
            ]
            .into_iter()
            .map(|(id, args)| ToolCall {
                id: id.into(),
                name: id.into(),
                args,
            })
            .collect();
            let reply = ModelReply {
                message: json!({"role":"assistant","content":""}),
                text: String::new(),
                calls,
            };
            let ToolMessages::Exchange(messages) =
                tool_messages(&provider, reply, WINDOW_MAX_BYTES, |call| {
                    Ok(ToolOutput::Temporary(call.args.clone()))
                })
                .unwrap()
            else {
                panic!("expected tool exchange");
            };
            let (results, id_key) = match provider {
                ProviderKind::OpenAi => (&messages[1..], "tool_call_id"),
                ProviderKind::Anthropic => (
                    messages[1]["content"].as_array().unwrap().as_slice(),
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
            assert_eq!(result(0)["id"], 4);
            assert_eq!(result(0)["outcome"]["status"], "failed");
            assert_eq!(result(1)["id"], 1);
            assert_eq!(result(2)["status"], "kept");
            assert_eq!(result(2)["fact_id"], 2);
            assert_eq!(result(2)["concern"], 2);
        }
    }

    #[test]
    fn project_window_enforces_budget_even_with_large_open_concerns() -> eja_core::Result<()> {
        let eja = Eja::open(":memory:")?;
        let payload = "a".repeat(60_000);
        for index in 0..5 {
            eja.keep(None, format!("concern-{index}-{payload}"))?;
        }
        for index in 0..3 {
            eja.notice(Source::World, format!("log-{index}-{payload}"))?;
        }
        let trigger = eja.notice(Source::Person, "trigger after large concerns")?;

        let mut anchor = 0;
        let window = project_window_through(&eja, &mut anchor, trigger, WINDOW_MAX_BYTES, trigger)?;
        assert!(anchor >= 5);
        assert!(
            messages_bytes(&window) <= WINDOW_MAX_BYTES,
            "projected window exceeded budget: {}",
            messages_bytes(&window)
        );
        let header: Value = serde_json::from_str(window[0]["content"].as_str().unwrap()).unwrap();
        let concerns = header["concerns"].as_array().unwrap();
        assert_eq!(concerns.len(), 5);
        assert!(concerns.iter().all(|item| {
            item["latest"].as_i64().is_some() && item["truncated"].as_bool() == Some(true)
        }));
        Ok(())
    }

    #[test]
    fn cap_tool_result_and_format_read_result_obey_budget_and_paginate() -> eja_core::Result<()> {
        let quotes = json!({ "payload": "\"".repeat(400) });
        let capped = cap_tool_result(&ProviderKind::OpenAi, quotes, 180);
        assert!(capped.to_string().len() <= 180);
        assert_eq!(capped["truncated"], true);

        let eja = Eja::open(":memory:")?;
        let large_text = "文".repeat(18_000);
        let fact_id = eja.notice(Source::World, &large_text)?;
        let evidence = eja.evidence(fact_id)?;

        let full = format_read_result(&ProviderKind::OpenAi, &evidence, 0, 128 * 1024);
        assert_eq!(full["id"], fact_id);
        assert_eq!(full["fact"]["text"], large_text);

        let mut offset = 0usize;
        let mut reassembled = String::new();
        for _ in 0..128 {
            let page = format_read_result(&ProviderKind::OpenAi, &evidence, offset, 2048);
            assert!(page.to_string().len() <= 2048);
            reassembled.push_str(page["chunk"].as_str().unwrap());
            if page["truncated"] == false {
                break;
            }
            let next = page["next_offset"].as_u64().unwrap() as usize;
            assert!(next > offset);
            offset = next;
        }
        let parsed: Value = serde_json::from_str(&reassembled).unwrap();
        assert_eq!(parsed["id"], fact_id);
        assert_eq!(parsed["fact"]["text"], large_text);
        Ok(())
    }

    #[test]
    fn tiny_windows_bound_the_protected_message() -> eja_core::Result<()> {
        let eja = Eja::open(":memory:")?;
        let id = eja.notice(Source::Person, "用户消息")?;
        for budget in 1..=4 {
            let window = project_window_through(&eja, &mut 0, id, budget, id)?;
            assert_eq!(window.len(), 1);
            assert!(messages_bytes(&window) <= budget);
        }
        Ok(())
    }

    #[test]
    fn project_window_through_preserves_protected_trigger_and_handles_many_concerns(
    ) -> eja_core::Result<()> {
        let eja = Eja::open(":memory:")?;
        for index in 0..400 {
            eja.keep(None, format!("concern-{index}-{}", "x".repeat(80)))?;
        }
        let trigger = eja.notice(Source::Person, "keep this unfinished user request")?;
        let act = eja.act(
            trigger,
            "shell",
            json!({ "command": "echo hi" }),
            &["shell"],
        )?;
        eja.finish(act.id, eja_core::Outcome::Observed("y".repeat(2000)))?;

        let mut anchor = 0;
        let head = eja.revision()?;
        let window = project_window_through(&eja, &mut anchor, head, 1200, trigger)?;
        assert!(
            anchor < trigger,
            "anchor {anchor} moved past trigger {trigger}"
        );
        assert!(messages_bytes(&window) <= 1200);
        assert!(window.iter().any(|msg| {
            msg["content"]
                .as_str()
                .is_some_and(|text| text.contains("keep this unfinished user request"))
        }));
        Ok(())
    }
}
