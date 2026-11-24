use crate::config::{ProviderConfig, ProviderKind};
use crate::hands::ToolSpec;
use crate::http::{get_bytes, post_stream, truncate_utf8, HttpError};
use crate::state::entries_for_turn;
use base64::{engine::general_purpose::STANDARD, Engine};
use eja_core::{Eja, Entry, Evidence, Fact, Id, Outcome};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};

pub const WINDOW_MAX_BYTES: usize = 256 * 1024;
pub const MAX_TOOL_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const TOOL_IMAGE_CONTEXT: &str = "图像来自工具调用 ";

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

    fn complete_streaming(
        &self,
        provider: &ProviderConfig,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelReply, String> {
        let reply = self.complete(provider, model, system, messages, tools)?;
        if !reply.text.is_empty() {
            on_delta(&reply.text);
        }
        Ok(reply)
    }

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
        complete_wire(provider, model, system, messages, Some(tools), None)
    }

    fn complete_streaming(
        &self,
        provider: &ProviderConfig,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelReply, String> {
        complete_wire(
            provider,
            model,
            system,
            messages,
            Some(tools),
            Some(on_delta),
        )
    }

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String> {
        let text = complete_wire(
            provider,
            model,
            "Reply with OK.",
            &[user_message("ping")],
            None,
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
    Images(Value, Vec<ModelImage>),
}

pub struct ModelImage {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

impl ModelImage {
    pub fn receipt(&self) -> String {
        format!(
            "工具返回图像（{}，{} 字节）",
            self.media_type,
            self.bytes.len()
        )
    }

    pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some("image/jpeg")
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some("image/webp")
        } else {
            None
        }
    }

    fn into_content_part(self, provider: &ProviderKind) -> Value {
        let data = STANDARD.encode(self.bytes);
        match provider {
            ProviderKind::OpenAi => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{data}", self.media_type) }
            }),
            ProviderKind::Anthropic => json!({
                "type": "image",
                "source": { "type": "base64", "media_type": self.media_type, "data": data }
            }),
        }
    }
}

pub fn tool_messages(
    provider: &ProviderKind,
    reply: ModelReply,
    window_bytes: usize,
    mut execute: impl FnMut(&ToolCall) -> Result<ToolOutput, String>,
) -> Result<Option<Vec<Value>>, String> {
    let count = reply.calls.len();
    if count == 0 {
        return Ok(None);
    }
    let mut slots: Vec<_> = reply
        .calls
        .iter()
        .map(|call| match provider {
            ProviderKind::OpenAi => json!({"role":"tool", "tool_call_id":call.id, "content":""}),
            ProviderKind::Anthropic => {
                json!({"type":"tool_result", "tool_use_id":call.id, "content":""})
            }
        })
        .collect();
    let reserve = (window_bytes / 4).min(64 * 1024);
    let budget = window_bytes.saturating_sub(reserve + msg_bytes(&reply.message));
    let framing = match provider {
        ProviderKind::OpenAi => messages_bytes(&slots),
        ProviderKind::Anthropic => msg_bytes(&json!({"role":"user", "content":slots})),
    };
    let per_call = budget.saturating_sub(framing) / count;
    let keep_exchange = per_call >= 128;
    let mut attachments = Vec::new();
    for (index, (call, slot)) in reply.calls.into_iter().zip(&mut slots).enumerate() {
        let output = execute(&call)?;
        if !keep_exchange {
            continue;
        }
        let value = match output {
            ToolOutput::Read(evidence, offset) => {
                format_read_result(provider, &evidence, offset, per_call)
            }
            ToolOutput::Value(v) => cap_tool_result(provider, v, per_call),
            ToolOutput::Images(value, images) => {
                let metadata = json!({"images_omitted": images.len()});
                let text_budget = per_call.saturating_sub(json_bytes(&metadata) + 2);
                let mut bounded = cap_tool_result(provider, value, text_budget);
                bounded["images_omitted"] = metadata["images_omitted"].clone();
                attachments.push((index, call.id, images, bounded.clone()));
                bounded
            }
        };
        slot["content"] = Value::String(value.to_string());
    }
    if !keep_exchange {
        return Ok(None);
    }
    let text_bytes = match provider {
        ProviderKind::OpenAi => messages_bytes(&slots),
        ProviderKind::Anthropic => msg_bytes(&json!({"role":"user", "content":slots})),
    };
    let mut available = budget.saturating_sub(text_bytes);
    let mut following_images = Vec::new();
    for (index, id, images, mut receipt) in attachments {
        let slot = &mut slots[index];
        let text = &slot["content"];
        let openai = *provider == ProviderKind::OpenAi;
        let mut parts = vec![if openai {
            json!({ "type": "text", "text": format!("{TOOL_IMAGE_CONTEXT}{id} 的回执。") })
        } else {
            json!({ "type": "text", "text": text })
        }];
        let framing = if openai {
            msg_bytes(&json!({"role":"user", "content":parts}))
        } else {
            json_bytes(&json!(parts)) - json_bytes(text)
        };
        let mut remaining = available.saturating_sub(framing);
        let total = images.len();
        for image in images {
            if image.bytes.len().div_ceil(3) * 4 >= remaining {
                continue;
            }
            let part = image.into_content_part(provider);
            let size = json_bytes(&part) + 1;
            if size <= remaining {
                remaining -= size;
                parts.push(part);
            }
        }
        if parts.len() == 1 {
            continue;
        }
        available = remaining;
        let omitted = total + 1 - parts.len();
        if let Some(receipt) = receipt.as_object_mut() {
            if omitted == 0 {
                receipt.remove("images_omitted");
            } else {
                receipt.insert("images_omitted".into(), json!(omitted));
            }
        }
        let text = receipt.to_string();
        if openai {
            slot["content"] = Value::String(text);
            following_images.push(json!({"role":"user", "content":parts}));
        } else {
            parts[0]["text"] = Value::String(text);
            slot["content"] = Value::Array(parts);
        }
    }
    let mut tail = vec![reply.message];
    match provider {
        ProviderKind::OpenAi => {
            tail.extend(slots);
            tail.extend(following_images);
        }
        ProviderKind::Anthropic => tail.push(json!({"role":"user", "content":slots})),
    }
    Ok(Some(tail))
}

pub fn expire_tool_images(messages: &mut Vec<Value>, provider: &ProviderKind) {
    match provider {
        ProviderKind::OpenAi => messages.retain(|message| {
            message["role"] != "user"
                || !message["content"].as_array().is_some_and(|parts| {
                    parts
                        .first()
                        .and_then(|part| part["text"].as_str())
                        .is_some_and(|text| text.starts_with(TOOL_IMAGE_CONTEXT))
                        && parts.iter().any(|part| part["type"] == "image_url")
                })
        }),
        ProviderKind::Anthropic => {
            for message in messages {
                if message["role"] != "user" {
                    continue;
                }
                if let Some(blocks) = message["content"].as_array_mut() {
                    for result in blocks
                        .iter_mut()
                        .filter(|block| block["type"] == "tool_result")
                    {
                        let text = result["content"]
                            .as_array()
                            .filter(|parts| parts.iter().any(|part| part["type"] == "image"))
                            .and_then(|parts| parts.first()?.get("text")?.as_str());
                        if let Some(text) = text {
                            result["content"] = Value::String(text.into());
                        }
                    }
                }
            }
        }
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
    let receipt_id = value.get("id").and_then(Value::as_i64);
    let action_id = value.get("action").and_then(Value::as_i64);
    let outcome_status = value
        .pointer("/outcome/status")
        .and_then(Value::as_str)
        .map(str::to_string);
    let make = |preview: &str| {
        let mut obj = json!({
            "truncated": true,
            "total_bytes": total_bytes,
            "preview": preview
        });
        if let Some(id) = receipt_id {
            obj["id"] = json!(id);
        }
        if let Some(action) = action_id {
            obj["action"] = json!(action);
        }
        if let Some(ref status) = outcome_status {
            obj["outcome_status"] = json!(status);
        }
        obj
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
        return json_bytes(msg) + 1;
    }
    msg["content"]
        .as_str()
        .map(|text| text.len() + 1)
        .unwrap_or_else(|| json_bytes(msg) + 1)
}

fn json_bytes(value: &Value) -> usize {
    struct ByteCounter(usize);

    impl Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).expect("counting JSON bytes cannot fail");
    counter.0
}

fn bound_entry_message(entry: &Entry, message: Value, budget: usize) -> Value {
    if msg_bytes(&message) <= budget {
        return message;
    }
    let content_budget = budget.saturating_sub(1);
    let role = message["role"].as_str().unwrap_or("user");
    let raw = message["content"].as_str().unwrap_or("");
    let suffix = match &entry.fact {
        Fact::Returned { outcome, .. } => {
            let status = match outcome {
                Outcome::Observed(_) => "observed",
                Outcome::Failed(_) => "failed",
                Outcome::Unknown(_) => "unknown",
            };
            format!(" [status={status}; truncated; read fact {}]", entry.id)
        }
        _ => format!(" [truncated; read fact {}]", entry.id),
    };
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
    let unknowns: Vec<Value> = eja
        .unsettled_unknowns(anchor, 8)?
        .into_iter()
        .filter_map(|entry| match entry.fact {
            Fact::Returned {
                action,
                outcome: Outcome::Unknown(detail),
            } => Some(json!({
                "id": entry.id,
                "action": action,
                "at": entry.at,
                "detail": truncate_utf8(&detail, 120)
            })),
            _ => None,
        })
        .collect();
    let encode = |items: &[Value], omitted: usize, unk: &[Value]| {
        let mut obj = json!({ "anchor": anchor, "concerns": items });
        if omitted > 0 {
            obj["omitted_concerns"] = json!(omitted);
        }
        if !unk.is_empty() {
            obj["unsettled_unknowns"] = json!(unk);
        }
        obj.to_string()
    };
    if content_budget == 0 || encode(&[], 0, &[]).len() > content_budget {
        return Ok(None);
    }
    let pack_with_unknowns = |items: &[Value], omitted: usize| {
        let mut kept: Vec<Value> = Vec::new();
        for item in unknowns.iter().rev() {
            kept.insert(0, item.clone());
            if encode(items, omitted, &kept).len() > content_budget {
                kept.remove(0);
                break;
            }
        }
        encode(items, omitted, &kept)
    };
    let concerns = eja.concerns(anchor)?;
    let full_items: Vec<Value> = concerns
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<_>>()?;
    if encode(&full_items, 0, &[]).len() <= content_budget {
        return Ok(Some(user_message(pack_with_unknowns(&full_items, 0))));
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
            if encode(&packed, rest, &[]).len() <= content_budget {
                continue;
            }
            packed.pop();
        }
        packed.push(trunc(""));
        let fits_empty = encode(&packed, rest, &[]).len() <= content_budget;
        packed.pop();
        if !fits_empty {
            break;
        }
        let prefix = fit_utf8_prefix(&concern.text, max_each, |slice| {
            packed.push(trunc(slice));
            let ok = encode(&packed, rest, &[]).len() <= content_budget;
            packed.pop();
            ok
        });
        packed.push(trunc(prefix));
    }
    let mut rendered = pack_with_unknowns(&packed, concerns.len() - packed.len());
    if rendered.len() > content_budget {
        rendered = encode(&packed, 0, &[]);
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

fn provider_headers(provider: &ProviderConfig) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".into(), "application/json".into())];
    let key = provider.api_key.trim();
    match provider.kind {
        ProviderKind::OpenAi if !key.is_empty() => {
            headers.push(("Authorization".into(), format!("Bearer {key}")));
        }
        ProviderKind::Anthropic => {
            headers.push(("anthropic-version".into(), "2023-06-01".into()));
            if !key.is_empty() {
                headers.push(("x-api-key".into(), key.into()));
            }
        }
        ProviderKind::OpenAi => {}
    }
    headers
}

pub fn list_models(provider: &ProviderConfig) -> Result<Vec<String>, String> {
    let url = format!("{}/models", provider.api_root()?);
    let response = get_bytes(&url, &provider_headers(provider), 8, false)
        .map_err(|err| format!("读取模型列表失败：{}", http_error_message(err)))?;
    if !(200..300).contains(&response.status) {
        return Err(format!("读取模型列表失败：HTTP {}", response.status));
    }
    let payload: Value = serde_json::from_str(&response.body)
        .map_err(|err| format!("模型列表不是有效 JSON：{err}"))?;
    let items = payload
        .as_array()
        .or_else(|| payload["data"].as_array())
        .or_else(|| payload["models"].as_array())
        .ok_or("模型服务未返回列表")?;
    let mut seen = HashSet::new();
    Ok(items
        .iter()
        .filter_map(|item| item["id"].as_str().or_else(|| item["name"].as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty() && seen.insert(*id))
        .map(str::to_string)
        .collect())
}

fn complete_wire(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[ToolSpec]>,
    on_delta: Option<&mut dyn FnMut(&str)>,
) -> Result<ModelReply, String> {
    match provider.kind {
        ProviderKind::OpenAi => call_openai(provider, model, system, messages, tools, on_delta),
        ProviderKind::Anthropic => {
            call_anthropic(provider, model, system, messages, tools, on_delta)
        }
    }
}

fn call_openai(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[ToolSpec]>,
    mut on_delta: Option<&mut dyn FnMut(&str)>,
) -> Result<ModelReply, String> {
    let url = format!("{}/chat/completions", provider.api_root()?);
    let system_message = json!({ "role": "system", "content": system });
    let messages: Vec<_> = std::iter::once(&system_message).chain(messages).collect();
    let seed = messages.len();
    let mut body = json!({ "model": model, "messages": messages });
    if let Some(list) = tools {
        body["tools"] = json!(list
            .iter()
            .map(|s| json!({"type": "function", "function": {"name": s.name, "description": s.description, "parameters": s.schema}}))
            .collect::<Vec<_>>());
    }
    let headers = provider_headers(provider);
    if on_delta.is_some() {
        body["stream"] = json!(true);
    }
    let mut notify = |chunk: &str| {
        if let Some(cb) = on_delta.as_deref_mut() {
            cb(chunk);
        }
    };
    let mut acc = OpenAiStreamAcc::default();
    let resp = post_json(&url, &headers, &body, |line| {
        acc.feed_line(line, &mut notify);
    })?;
    if !acc.saw_sse {
        acc.apply_response(&resp, &mut notify)?;
    }
    acc.into_reply(seed)
}

#[derive(Default)]
struct OpenAiStreamAcc {
    saw_sse: bool,
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, (String, String, String)>,
    stream_error: Option<String>,
}

impl OpenAiStreamAcc {
    fn apply_response(&mut self, resp: &Value, on_delta: impl FnMut(&str)) -> Result<(), String> {
        let msg = resp.pointer("/choices/0/message").ok_or_else(|| {
            format!(
                "LLM 响应缺少 choices[0].message: {}",
                truncate_utf8(&resp.to_string(), 240)
            )
        })?;
        self.apply_delta(msg, on_delta);
        Ok(())
    }

    fn feed_line(&mut self, line: &str, on_delta: impl FnMut(&str)) {
        let Some(chunk) = parse_sse_json(line, &mut self.saw_sse) else {
            return;
        };
        if chunk.get("choices").is_none() {
            if let Some(msg) = chunk
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| chunk.get("error").and_then(Value::as_str))
            {
                self.stream_error = Some(msg.to_string());
            }
            return;
        }
        if let Some(delta) = chunk
            .pointer("/choices/0/delta")
            .or_else(|| chunk.pointer("/choices/0/message"))
        {
            self.apply_delta(delta, on_delta);
        }
    }

    fn apply_delta(&mut self, delta: &Value, mut on_delta: impl FnMut(&str)) {
        if let Some(r) = delta["reasoning_content"].as_str() {
            self.reasoning.push_str(r);
        }
        if let Some(c) = delta["content"].as_str().filter(|s| !s.is_empty()) {
            self.text.push_str(c);
            on_delta(c);
        }
        if let Some(arr) = delta["tool_calls"].as_array() {
            for (pos, item) in arr.iter().enumerate() {
                let idx = item["index"].as_u64().map_or(pos, |i| i as usize);
                let (id, name, arguments) = self.tool_calls.entry(idx).or_default();
                if let Some(s) = item["id"].as_str().filter(|s| !s.is_empty()) {
                    *id = s.to_string();
                }
                if let Some(s) = item.pointer("/function/name").and_then(Value::as_str) {
                    name.push_str(s);
                }
                match item.pointer("/function/arguments") {
                    Some(Value::String(s)) => arguments.push_str(s),
                    Some(v) if v.is_object() => arguments.push_str(&v.to_string()),
                    _ => {}
                }
            }
        }
    }

    fn into_reply(self, seed: usize) -> Result<ModelReply, String> {
        if let Some(err) = self.stream_error {
            return Err(err);
        }
        let content_val = if self.text.is_empty() && !self.tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(self.text.clone())
        };
        let mut message = json!({ "role": "assistant", "content": content_val });
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(self.reasoning);
        }
        let mut calls = Vec::new();
        let mut wire_calls = Vec::new();
        for (index, (raw_id, name, raw_args)) in self.tool_calls.into_values().enumerate() {
            if name.is_empty() {
                continue;
            }
            let id = if raw_id.is_empty() {
                format!("call_{seed}_{index}")
            } else {
                raw_id
            };
            let arguments = if raw_args.trim().is_empty() {
                "{}".to_string()
            } else {
                raw_args
            };
            let args = serde_json::from_str(&arguments).map_err(|err| {
                format!(
                    "模型返回的工具参数不是有效 JSON（{name}）：{err}（{}）",
                    truncate_utf8(arguments.trim(), 120)
                )
            })?;
            wire_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            }));
            calls.push(ToolCall { id, name, args });
        }
        if !wire_calls.is_empty() {
            message["tool_calls"] = json!(wire_calls);
        }
        Ok(ModelReply {
            message,
            text: self.text,
            calls,
        })
    }
}

fn call_anthropic(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    messages: &[Value],
    tools: Option<&[ToolSpec]>,
    mut on_delta: Option<&mut dyn FnMut(&str)>,
) -> Result<ModelReply, String> {
    let url = format!("{}/messages", provider.api_root()?);
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
    let headers = provider_headers(provider);
    if on_delta.is_some() {
        body["stream"] = json!(true);
    }
    let mut notify = |chunk: &str| {
        if let Some(cb) = on_delta.as_deref_mut() {
            cb(chunk);
        }
    };
    let mut acc = AnthropicStreamAcc::default();
    let resp = post_json(&url, &headers, &body, |line| {
        acc.feed_line(line, &mut notify);
    })?;
    if acc.saw_sse {
        return acc.into_reply();
    }
    let reply = parse_anthropic_json(&resp, None)?;
    if !reply.text.is_empty() {
        notify(&reply.text);
    }
    Ok(reply)
}

fn parse_anthropic_json(
    resp: &Value,
    override_blocks: Option<Vec<Value>>,
) -> Result<ModelReply, String> {
    let blocks = match override_blocks {
        Some(b) => b,
        None => resp
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Anthropic 响应缺少 content: {}",
                    truncate_utf8(&resp.to_string(), 240)
                )
            })?,
    };
    let mut texts = Vec::new();
    let mut calls = Vec::new();
    for block in &blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    texts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                    if !id.is_empty() && !name.is_empty() {
                        calls.push(ToolCall {
                            id: id.into(),
                            name: name.into(),
                            args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                        });
                    }
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

#[derive(Default)]
struct AnthropicStreamAcc {
    saw_sse: bool,
    blocks: BTreeMap<usize, Value>,
    partial_inputs: BTreeMap<usize, String>,
    stream_error: Option<String>,
}

impl AnthropicStreamAcc {
    fn feed_line(&mut self, line: &str, mut on_delta: impl FnMut(&str)) {
        let Some(ev) = parse_sse_json(line, &mut self.saw_sse) else {
            return;
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("error") => {
                let msg = ev
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| ev.get("message").and_then(Value::as_str))
                    .unwrap_or("Anthropic 流式响应错误");
                self.stream_error = Some(msg.to_string());
            }
            Some("content_block_start") => {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(block) = ev.get("content_block") else {
                    return;
                };
                if let Some(t) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    on_delta(t);
                }
                self.blocks.insert(idx, block.clone());
            }
            Some("content_block_delta") => {
                let idx = ev.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let Some(delta) = ev.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            on_delta(t);
                            let entry = self
                                .blocks
                                .entry(idx)
                                .or_insert_with(|| json!({ "type": "text", "text": "" }));
                            let cur = entry.get("text").and_then(Value::as_str).unwrap_or("");
                            entry["text"] = Value::String(format!("{cur}{t}"));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(part) = delta.get("partial_json").and_then(Value::as_str) {
                            self.partial_inputs.entry(idx).or_default().push_str(part);
                        }
                    }
                    Some(kind @ ("thinking_delta" | "signature_delta")) => {
                        let field = if kind == "thinking_delta" {
                            "thinking"
                        } else {
                            "signature"
                        };
                        if let Some(part) = delta
                            .get(field)
                            .and_then(Value::as_str)
                            .filter(|chunk| !chunk.is_empty())
                        {
                            let entry = self
                                .blocks
                                .entry(idx)
                                .or_insert_with(|| json!({ "type": "thinking", "thinking": "" }));
                            let cur = entry.get(field).and_then(Value::as_str).unwrap_or("");
                            entry[field] = Value::String(format!("{cur}{part}"));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn into_reply(mut self) -> Result<ModelReply, String> {
        if let Some(err) = self.stream_error {
            return Err(err);
        }
        for (idx, raw) in self.partial_inputs {
            if !raw.trim().is_empty() {
                let parsed: Value = serde_json::from_str(&raw).map_err(|err| {
                    format!(
                        "模型返回的工具参数不是有效 JSON：{err}（{}）",
                        truncate_utf8(raw.trim(), 120)
                    )
                })?;
                if let Some(block) = self.blocks.get_mut(&idx) {
                    block["input"] = parsed;
                }
            }
        }
        parse_anthropic_json(&Value::Null, Some(self.blocks.into_values().collect()))
    }
}

fn parse_sse_json(line: &str, saw_sse: &mut bool) -> Option<Value> {
    let data = line.trim().strip_prefix("data:")?;
    *saw_sse = true;
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

fn post_json(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    on_line: impl FnMut(&str),
) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|err| err.to_string())?;
    let resp = post_stream(url, headers, &bytes, 300, on_line).map_err(http_error_message)?;
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

fn http_error_message(err: HttpError) -> String {
    match err {
        HttpError::Launch(msg) => msg,
        HttpError::Wait(msg) => format!("等待 LLM 响应失败: {msg}"),
        HttpError::Transport { message, .. } => format!("网络请求失败: {message}"),
        HttpError::InvalidResponse(msg) => format!("无效 LLM 响应: {msg}"),
    }
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
    fn tool_image_uses_the_models_image_input_and_respects_the_window() {
        for provider in [ProviderKind::OpenAi, ProviderKind::Anthropic] {
            let reply = ModelReply {
                message: json!({"role": "assistant", "content": ""}),
                text: String::new(),
                calls: vec![
                    ToolCall {
                        id: "image-call".into(),
                        name: "image-tool".into(),
                        args: json!({}),
                    },
                    ToolCall {
                        id: "text-call".into(),
                        name: "text-tool".into(),
                        args: json!({}),
                    },
                ],
            };
            let run = |budget, bytes| {
                tool_messages(&provider, reply.clone(), budget, |call| {
                    if call.name == "image-tool" {
                        Ok(ToolOutput::Images(
                            json!({"id":2,"action":1,"outcome":{"status":"observed","detail":"已发送输入"}}),
                            vec![ModelImage { media_type: "image/jpeg", bytes: vec![42; bytes] }],
                        ))
                    } else {
                        Ok(ToolOutput::Value(json!({"status":"ok"})))
                    }
                })
                .unwrap()
                .unwrap()
            };
            let mut exchange = run(4096, 1500);
            match provider {
                ProviderKind::OpenAi => {
                    assert_eq!(exchange[1]["tool_call_id"], "image-call");
                    assert_eq!(exchange[3]["role"], "user");
                    assert!(exchange[3]["content"][1]["image_url"]["url"]
                        .as_str()
                        .unwrap()
                        .starts_with("data:image/jpeg;base64,Kioq"));
                }
                ProviderKind::Anthropic => {
                    assert_eq!(exchange[1]["content"][0]["tool_use_id"], "image-call");
                    assert!(exchange[1]["content"][0]["content"][1]["source"]["data"]
                        .as_str()
                        .unwrap()
                        .starts_with("Kioq"));
                }
            }
            assert!(messages_bytes(&exchange) <= 4096);
            expire_tool_images(&mut exchange, &provider);
            assert!(!exchange
                .iter()
                .any(|message| message.to_string().contains("Kioq")));
            let bounded = run(1024, 1024);
            let raw = serde_json::to_string(&bounded).unwrap();
            assert!(raw.contains("images_omitted"));
            assert!(raw.contains("observed"));
            assert!(raw.contains("已发送输入"));
            assert!(!raw.contains("Kioq"));
            assert!(messages_bytes(&bounded) <= 1024);
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

    #[test]
    fn anchor_header_surfaces_unsettled_unknowns_and_capped_receipts_keep_status(
    ) -> eja_core::Result<()> {
        let eja = Eja::open(":memory:")?;
        let act = eja.act(eja.revision()?, "shell", json!({"command": "deploy"}))?;
        let unk = eja.finish(act.id, Outcome::Unknown("宿主中断未完成".into()))?;
        for i in 0..4 {
            eja.notice(Source::World, format!("filler-{i}-{}", "z".repeat(400)))?;
        }
        let trigger = eja.notice(Source::Person, "继续检查状态")?;
        let mut anchor = 0;
        let window = project_window_through(&eja, &mut anchor, trigger, 1024, trigger)?;
        assert!(anchor >= unk.id);
        let header: Value = serde_json::from_str(window[0]["content"].as_str().unwrap()).unwrap();
        let unknowns = header["unsettled_unknowns"].as_array().unwrap();
        assert_eq!(unknowns.len(), 1);
        assert_eq!(unknowns[0]["id"], unk.id);
        assert_eq!(unknowns[0]["action"], act.id);

        eja.finish(act.id, Outcome::Observed("deploy ok".into()))?;
        let trigger2 = eja.notice(Source::Person, "再看一次")?;
        let mut anchor2 = anchor;
        let window2 = project_window_through(&eja, &mut anchor2, trigger2, 1024, trigger2)?;
        assert_eq!(anchor2, anchor);
        assert_eq!(window2[0], window[0]);
        assert!(window2.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|s| s.contains("deploy ok"))));

        for i in 0..4 {
            eja.notice(
                Source::World,
                format!("after-settle-{i}-{}", "w".repeat(400)),
            )?;
        }
        let trigger3 = eja.notice(Source::Person, "重置后检查")?;
        let mut anchor3 = anchor2;
        let window3 = project_window_through(&eja, &mut anchor3, trigger3, 1024, trigger3)?;
        assert!(anchor3 > trigger2);
        let header3: Value = serde_json::from_str(window3[0]["content"].as_str().unwrap()).unwrap();
        assert!(header3.get("unsettled_unknowns").is_none());

        let capped = cap_tool_result(
            &ProviderKind::OpenAi,
            json!({
                "id": 99,
                "action": 98,
                "outcome": { "status": "unknown", "detail": "x".repeat(1000) }
            }),
            260,
        );
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["id"], 99);
        assert_eq!(capped["action"], 98);
        assert_eq!(capped["outcome_status"], "unknown");
        Ok(())
    }

    #[test]
    fn openai_stream_acc_collects_reasoning_text_and_chunked_tool_calls() {
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"先分析"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"再回答","content":"你好"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"，世界"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"sh","arguments":"{\"comm"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"ell","arguments":"and\":\"ls\"}"}}]}}]}"#,
            "data: [DONE]",
        ];
        let mut acc = OpenAiStreamAcc::default();
        let mut deltas = Vec::new();
        for line in lines {
            acc.feed_line(line, |d| deltas.push(d.to_string()));
        }
        assert!(acc.saw_sse);
        assert_eq!(deltas, vec!["你好", "，世界"]);
        let reply = acc.into_reply(3).unwrap();
        assert_eq!(reply.text, "你好，世界");
        assert_eq!(reply.message["reasoning_content"], "先分析再回答");
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].id, "call_1");
        assert_eq!(reply.calls[0].name, "shell");
        assert_eq!(reply.calls[0].args, json!({ "command": "ls" }));
    }

    #[test]
    fn anthropic_stream_acc_collects_text_and_input_json_deltas() {
        let lines = [
            r#"event: content_block_start"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"正在"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"检查"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"pwd\"}"}}"#,
            r#"data: {"type":"message_stop"}"#,
        ];
        let mut acc = AnthropicStreamAcc::default();
        let mut deltas = Vec::new();
        for line in lines {
            acc.feed_line(line, |d| deltas.push(d.to_string()));
        }
        assert!(acc.saw_sse);
        assert_eq!(deltas, vec!["正在", "检查"]);
        let reply = acc.into_reply().unwrap();
        assert_eq!(reply.text, "正在检查");
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].id, "toolu_1");
        assert_eq!(reply.calls[0].name, "shell");
        assert_eq!(reply.calls[0].args, json!({ "command": "pwd" }));
    }

    #[test]
    fn anthropic_stream_acc_preserves_thinking_and_signature_deltas() {
        let mut acc = AnthropicStreamAcc::default();
        let mut deltas = Vec::new();
        for line in [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"先检查"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"状态"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_part1"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"_part2"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_2","name":"shell","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pwd\"}"}}"#,
        ] {
            acc.feed_line(line, |chunk| deltas.push(chunk.to_string()));
        }
        assert!(deltas.is_empty());
        let reply = acc.into_reply().unwrap();
        assert_eq!(
            reply.message["content"][0],
            json!({"type": "thinking", "thinking": "先检查状态", "signature": "sig_part1_part2"})
        );
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].args, json!({"command": "pwd"}));
    }

    #[test]
    fn stream_accumulators_reject_truncated_tool_call_json() {
        let mut openai = OpenAiStreamAcc::default();
        openai.feed_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"command\":\"ls"}}]}}]}"#,
            |_| {},
        );
        assert!(openai.into_reply(1).is_err());

        let mut anthropic = AnthropicStreamAcc::default();
        anthropic.feed_line(
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"shell","input":{}}}"#,
            |_| {},
        );
        anthropic.feed_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            |_| {},
        );
        assert!(anthropic.into_reply().is_err());
    }
}
