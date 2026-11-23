use crate::config::{ProviderConfig, ProviderKind};
use crate::hands::ToolSpec;
use crate::http::{post_bytes, truncate_utf8, HttpError};
use crate::state::entries_for_turn;
use eja_core::{Eja, Entry, Evidence, Fact, Id};
use serde_json::{json, Value};

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
        complete_wire(provider, model, system, messages, Some(tools))
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
    Value(Value),
    Read(Evidence, usize),
}

pub fn tool_messages(
    provider: &ProviderKind,
    reply: ModelReply,
    window_bytes: usize,
    mut execute: impl FnMut(&ToolCall) -> Result<ToolOutput, String>,
) -> Result<Option<Vec<Value>>, String> {
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
    let keep_exchange = per_call >= 128;
    let slots = match provider {
        ProviderKind::OpenAi => tail.as_mut_slice(),
        ProviderKind::Anthropic => tail[0]["content"]
            .as_array_mut()
            .expect("tool result blocks")
            .as_mut_slice(),
    };
    for (call, slot) in reply.calls.into_iter().zip(slots) {
        let output = execute(&call)?;
        if !keep_exchange {
            continue;
        }
        let value = match output {
            ToolOutput::Read(evidence, offset) => {
                format_read_result(provider, &evidence, offset, per_call)
            }
            ToolOutput::Value(v) => cap_tool_result(provider, v, per_call),
        };
        slot["content"] = Value::String(value.to_string());
    }
    if keep_exchange {
        tail.insert(0, reply.message);
        Ok(Some(tail))
    } else {
        Ok(None)
    }
}

pub fn apply_tool_exchange(
    exchange: Option<Vec<Value>>,
    messages: &mut Vec<Value>,
    eja: &Eja,
    anchor: &mut Id,
    before: Id,
    trigger: Id,
    max_bytes: usize,
) -> eja_core::Result<()> {
    let (head, tail) = match exchange {
        Some(tail) if messages_bytes(messages) + messages_bytes(&tail) <= max_bytes => {
            messages.extend(tail);
            return Ok(());
        }
        Some(tail) => (before, tail),
        None => (eja.revision()?, Vec::new()),
    };
    let budget = max_bytes.saturating_sub(messages_bytes(&tail));
    let mut prefix = project_window_through(eja, anchor, head, budget, trigger)?;
    prefix.extend(tail);
    *messages = prefix;
    Ok(())
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
    let entries = entries_for_turn(eja, *anchor, through, trigger)?;
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

fn fit_utf8_prefix(text: &str, max_prefix: usize, mut fits: impl FnMut(&str) -> bool) -> &str {
    let mut hi = text.floor_char_boundary(max_prefix.min(text.len()));
    if fits(&text[..hi]) {
        return &text[..hi];
    }
    let mut lo = 0usize;
    while lo < hi {
        let mut mid = text.floor_char_boundary(lo + (hi - lo) / 2);
        if mid <= lo {
            mid = text.ceil_char_boundary(lo + 1);
            if mid >= hi {
                break;
            }
        }
        if fits(&text[..mid]) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    &text[..lo]
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
    let prefix = fit_utf8_prefix(&serialized, max_bytes, |slice| {
        result_fits(&make(slice).to_string(), provider, max_bytes)
    });
    make(prefix)
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
    let start = raw.floor_char_boundary(offset.min(raw.len()));
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
    let chunk = fit_utf8_prefix(remaining, max_bytes, |slice| {
        result_fits(&build_page(slice).to_string(), provider, max_bytes)
    });
    if chunk.is_empty() && !remaining.is_empty() {
        return cap_tool_result(
            provider,
            json!({"error": "窗口不足以容纳分页数据"}),
            max_bytes,
        );
    }
    build_page(chunk)
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
    let encode = |items: &[Value], omitted: usize| {
        let mut obj = json!({ "anchor": anchor, "concerns": items });
        if omitted > 0 {
            obj["omitted_concerns"] = json!(omitted);
        }
        obj.to_string()
    };
    if content_budget == 0 || encode(&[], 0).len() > content_budget {
        return Ok(None);
    }
    let concerns = eja.concerns(anchor)?;
    let full = serde_json::to_string(&json!({ "anchor": anchor, "concerns": concerns }))?;
    if full.len() <= content_budget {
        return Ok(Some(user_message(full)));
    }

    let max_each = (content_budget / concerns.len().max(1)).clamp(48, 2048);
    let mut packed: Vec<Value> = Vec::new();
    for (index, concern) in concerns.iter().enumerate() {
        let rest = concerns.len() - index - 1;
        let trunc = |text: &str| {
            json!({
                "id": concern.id, "latest": concern.latest, "at": concern.at,
                "text": text, "truncated": true
            })
        };
        if concern.text.len() <= max_each {
            packed.push(serde_json::to_value(concern)?);
            if encode(&packed, rest).len() <= content_budget {
                continue;
            }
            packed.pop();
        }
        packed.push(trunc(""));
        let fits_empty = encode(&packed, rest).len() <= content_budget;
        packed.pop();
        if !fits_empty {
            break;
        }
        let prefix = fit_utf8_prefix(&concern.text, max_each, |slice| {
            packed.push(trunc(slice));
            let ok = encode(&packed, rest).len() <= content_budget;
            packed.pop();
            ok
        });
        packed.push(trunc(prefix));
    }
    let mut rendered = encode(&packed, concerns.len() - packed.len());
    if rendered.len() > content_budget {
        rendered = encode(&packed, 0);
    }
    Ok(Some(user_message(rendered)))
}

fn render_entry(entry: &Entry) -> eja_core::Result<Value> {
    match &entry.fact {
        Fact::Said { text, .. } => Ok(json!({ "role": "assistant", "content": text })),
        _ => Ok(user_message(serde_json::to_string(entry)?)),
    }
}

fn user_message(content: impl Into<String>) -> Value {
    json!({ "role": "user", "content": content.into() })
}

fn complete_wire(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[ToolSpec]>,
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
    tools: Option<&[ToolSpec]>,
) -> Result<ModelReply, String> {
    let base = provider.base_url.trim().trim_end_matches('/');
    let url = if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    };
    let system_message = json!({ "role": "system", "content": system });
    let messages: Vec<_> = std::iter::once(&system_message).chain(messages).collect();
    let mut body = json!({ "model": model, "messages": messages });
    if let Some(list) = tools {
        body["tools"] = json!(list
            .iter()
            .map(|s| json!({"type": "function", "function": {"name": s.name, "description": s.description, "parameters": s.schema}}))
            .collect::<Vec<_>>());
    }
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    let api_key = provider.api_key.trim();
    if !api_key.is_empty() {
        headers.push(("Authorization".into(), format!("Bearer {api_key}")));
    }
    let resp = post_json(&url, &headers, &body)?;
    let msg = resp.pointer("/choices/0/message").ok_or_else(|| {
        format!(
            "LLM 响应缺少 choices[0].message: {}",
            truncate_utf8(&resp.to_string(), 240)
        )
    })?;
    let text = msg["content"].as_str().unwrap_or("").to_string();
    let mut message = json!({ "role": "assistant", "content": msg["content"] });
    if let Some(reasoning) = msg.get("reasoning_content").filter(|v| v.is_string()) {
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
    tools: Option<&[ToolSpec]>,
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
        "model": model, "max_tokens": 16384, "system": system, "messages": messages
    });
    if let Some(list) = tools {
        body["tools"] = json!(list
            .iter()
            .map(
                |s| json!({"name": s.name, "description": s.description, "input_schema": s.schema})
            )
            .collect::<Vec<_>>());
    }
    let headers = vec![
        ("Content-Type".into(), "application/json".into()),
        ("x-api-key".into(), provider.api_key.trim().into()),
        ("anthropic-version".into(), "2023-06-01".into()),
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
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    texts.push(text.to_string());
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
    let resp = post_bytes(url, headers, &bytes, 300).map_err(|err| match err {
        HttpError::Launch(msg) => msg,
        HttpError::Wait(msg) => format!("等待 LLM 响应失败: {msg}"),
        HttpError::Transport { message, .. } => format!("网络请求失败: {message}"),
        HttpError::InvalidResponse(msg) => format!("无效 LLM 响应: {msg}"),
    })?;
    let parsed: Value =
        serde_json::from_str(&resp.body).unwrap_or_else(|_| json!({ "raw": resp.body }));
    if !(200..300).contains(&resp.status) {
        let detail = parsed
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or(&resp.body);
        return Err(format!(
            "HTTP {}: {}",
            resp.status,
            truncate_utf8(detail, 280)
        ));
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
            let Some(messages) = tool_messages(&provider, reply, WINDOW_MAX_BYTES, |call| {
                Ok(ToolOutput::Value(call.args.clone()))
            })
            .unwrap() else {
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
        let act = eja.act(trigger, "shell", json!({ "command": "echo hi" }))?;
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
