use crate::config::{ProviderConfig, ProviderKind};
use crate::hands::ToolSpec;
use crate::mcp::truncate_utf8;
use eja_core::{Eja, Entry, Fact, Id};
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

pub fn append_tool_results(
    provider: &ProviderKind,
    messages: &mut Vec<Value>,
    results: Vec<(String, Value)>,
) {
    match provider {
        ProviderKind::OpenAi => {
            for (id, value) in results {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": value.to_string(),
                }));
            }
        }
        ProviderKind::Anthropic => {
            let blocks: Vec<Value> = results
                .into_iter()
                .map(|(id, value)| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": value.to_string(),
                    })
                })
                .collect();
            if !blocks.is_empty() {
                messages.push(json!({ "role": "user", "content": blocks }));
            }
        }
    }
}

/// Project the immutable Core journal from `anchor` through `head` into a strictly append-only
/// sequence of immutable messages.
/// Advances `anchor` only when the projected window exceeds `max_bytes`.
pub fn project_window(
    eja: &Eja,
    anchor: &mut Id,
    max_bytes: usize,
) -> eja_core::Result<Vec<Value>> {
    let head = eja.revision()?;
    if head == 0 {
        *anchor = 0;
        return Ok(Vec::new());
    }
    if *anchor > head {
        *anchor = 0;
    }
    let entries = eja.entries_after(*anchor, head)?;
    let entry_msgs: Vec<Value> = entries
        .iter()
        .map(render_entry)
        .collect::<eja_core::Result<_>>()?;
    let header = if *anchor > 0 {
        Some(render_anchor_header(eja, *anchor)?)
    } else {
        None
    };
    let total_bytes =
        header.as_ref().map_or(0, msg_bytes) + entry_msgs.iter().map(msg_bytes).sum::<usize>();
    if total_bytes <= max_bytes {
        let mut out = Vec::with_capacity(usize::from(header.is_some()) + entry_msgs.len());
        if let Some(h) = header {
            out.push(h);
        }
        out.extend(entry_msgs);
        return Ok(out);
    }

    let target_bytes = max_bytes / 2;
    let mut kept_bytes = 0usize;
    let mut keep_from = entries.len().saturating_sub(1);
    for idx in (0..entries.len()).rev() {
        let line_len = msg_bytes(&entry_msgs[idx]);
        if idx < entries.len() - 1 && kept_bytes + line_len > target_bytes {
            break;
        }
        kept_bytes += line_len;
        keep_from = idx;
    }
    if keep_from > 0 {
        *anchor = entries[keep_from - 1].id;
        let mut out = Vec::with_capacity(1 + entry_msgs.len() - keep_from);
        out.push(render_anchor_header(eja, *anchor)?);
        out.extend(entry_msgs.into_iter().skip(keep_from));
        Ok(out)
    } else {
        let mut out = Vec::with_capacity(usize::from(header.is_some()) + entry_msgs.len());
        if let Some(h) = header {
            out.push(h);
        }
        out.extend(entry_msgs);
        Ok(out)
    }
}

fn msg_bytes(msg: &Value) -> usize {
    msg["content"].as_str().map_or(0, str::len) + 1
}

fn render_anchor_header(eja: &Eja, anchor: Id) -> eja_core::Result<Value> {
    let concerns = eja.concerns(anchor)?;
    Ok(user_message(serde_json::to_string(&json!({
        "anchor": anchor,
        "concerns": concerns
    }))?))
}

fn render_entry(entry: &Entry) -> eja_core::Result<Value> {
    match &entry.fact {
        Fact::Said { text } => Ok(json!({ "role": "assistant", "content": text })),
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
        eja.notice(Source::Person, "first notice")?;
        let w1 = project_window(&eja, &mut anchor, 4096)?;
        assert_eq!(anchor, 0);

        eja.keep(None, "track workshop labels")?;
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
    fn append_tool_results_preserves_order_and_ids_in_both_protocols() {
        for provider in [ProviderKind::OpenAi, ProviderKind::Anthropic] {
            let mut messages = vec![user_message("start")];
            append_tool_results(
                &provider,
                &mut messages,
                vec![
                    (
                        "action".into(),
                        json!({ "id": 4, "outcome": { "status": "failed" } }),
                    ),
                    ("read".into(), json!({ "id": 1 })),
                    (
                        "keep".into(),
                        json!({ "status": "kept", "concern": 2, "fact_id": 2 }),
                    ),
                ],
            );
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
}
