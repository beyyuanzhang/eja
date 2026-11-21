use crate::config::{ProviderConfig, ProviderKind};
use crate::mcp::{truncate_utf8, McpTool};
use eja_core::{Act, Consolidation, Fact, Keep, Source, Step, View};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};

pub trait Brain: Send + Sync {
    fn judge(
        &self,
        provider: &ProviderConfig,
        model: &str,
        view: &View,
        mcp_tools: &[McpTool],
        has_encoder: bool,
        local_time: &str,
        data_dir: &str,
    ) -> Result<Step, String>;

    fn distill(
        &self,
        provider: &ProviderConfig,
        model: &str,
        window: &Consolidation,
    ) -> Result<String, String>;

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String>;
}

pub struct HttpBrain;

impl Brain for HttpBrain {
    fn judge(
        &self,
        provider: &ProviderConfig,
        model: &str,
        view: &View,
        mcp_tools: &[McpTool],
        has_encoder: bool,
        local_time: &str,
        data_dir: &str,
    ) -> Result<Step, String> {
        let (defs, alias_to_cap) = build_tool_specs(mcp_tools, has_encoder);
        let earliest_recent = view.recent.iter().map(|entry| entry.id).min();
        let has_earlier = earliest_recent.is_some_and(|id| id > 1);
        let search_note = if has_encoder {
            "支持 `search(cue)` 语义检索全部历史。"
        } else {
            "当前未挂载语义编码器，请勿调用 `search`；需要翻看更早日志时用 `read(fact)`。"
        };
        let system = format!(
            "你是 EJA，一个持续存在的自我同一性主体。\n\
            本机当前时间：{local_time}\n\
            数据目录：{data_dir}（配置文件：{data_dir}/config.json）\n\n\
            记忆规则：\n\
            - `view.recent` 只展示最近最多 8 条日志，不代表之前的历史不存在；若 `has_earlier_history` 为 true，说明存在更早日志（ID 1..{}），可用 `read(fact)` 回查，绝不要编造“没有更早记录”。\n\
            - {search_note}\n\
            - 用 `keep` / `release` 维护跨回合挂念。\n\n\
            行动规则：\n\
            - 回复人请调用 `say(text)`（支持 Markdown）；主动定时提醒请调用 `wake(delay_seconds, reason)`。\n\
            - `shell(command)` 可在本机执行命令、读写文件；若需要新能力，可编写标准 stdio MCP Server 并写入 `{data_dir}/config.json` 的 `connectors` 中，保存后会自动热加载。\n\
            - 所有已连接的 MCP 工具均以 `mcp.*` 形式直接可用。完成后不调工具即可自然结束回合。",
            earliest_recent.unwrap_or(1).saturating_sub(1)
        );

        let user_payload = json!({
            "local_time": local_time,
            "has_earlier_history": has_earlier,
            "earliest_recent_id": earliest_recent,
            "view": view
        });
        let user_text = serde_json::to_string(&user_payload).map_err(|err| err.to_string())?;
        let (text, calls) = match provider.kind {
            ProviderKind::OpenAi => call_openai(provider, model, &system, &user_text, Some(&defs))?,
            ProviderKind::Anthropic => {
                call_anthropic(provider, model, &system, &user_text, Some(&defs))?
            }
        };

        let mut step = Step::default();
        for (name, args) in calls {
            match name.as_str() {
                "keep" => {
                    let text = args
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() {
                        step.keep.push(Keep {
                            concern: args.get("concern").and_then(|v| v.as_i64()),
                            text: text.to_string(),
                        });
                    }
                }
                "release" => {
                    if let Some(id) = args.get("concern").and_then(|v| v.as_i64()) {
                        step.release.push(id);
                    }
                }
                "read" => {
                    if let Some(id) = args.get("fact").and_then(|v| v.as_i64()) {
                        step.read.push(id);
                    }
                }
                "search" if has_encoder => {
                    let cue = args
                        .get("cue")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if !cue.is_empty() {
                        step.search.push(cue.to_string());
                    }
                }
                "say" | "wake" | "shell" => {
                    step.act.push(Act {
                        capability: name,
                        input: args,
                    });
                }
                other => {
                    if let Some(cap) = alias_to_cap.get(other) {
                        step.act.push(Act {
                            capability: cap.clone(),
                            input: args,
                        });
                    }
                }
            }
        }

        if !step.has_mutations() && !step.needs_followup() {
            let trimmed = text.trim();
            if !trimmed.is_empty() && latest_notice_needs_reply(view) {
                step.act.push(Act {
                    capability: "say".into(),
                    input: json!({ "text": trimmed }),
                });
            }
        }
        Ok(step)
    }

    fn distill(
        &self,
        provider: &ProviderConfig,
        model: &str,
        window: &Consolidation,
    ) -> Result<String, String> {
        let system = "你是 EJA 的长期记忆整理器。把之前的长期记忆 `previous` 与本批日志 `entries` 融合为一份精炼、准确的纯文本长期记忆，保留用户身份、偏好、约定、长期事实与重要决定，去掉寒暄和过程噪音。直接输出记忆正文，不要 Markdown 外壳。";
        let user_text = serde_json::to_string(window).map_err(|err| err.to_string())?;
        let (text, _) = match provider.kind {
            ProviderKind::OpenAi => call_openai(provider, model, system, &user_text, None)?,
            ProviderKind::Anthropic => call_anthropic(provider, model, system, &user_text, None)?,
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("记忆整理返回为空".into());
        }
        Ok(truncate_utf8(trimmed, 7000))
    }

    fn test_connection(&self, provider: &ProviderConfig, model: &str) -> Result<String, String> {
        let (text, _) = match provider.kind {
            ProviderKind::OpenAi => {
                call_openai(provider, model, "Reply with OK.", "ping", None)?
            }
            ProviderKind::Anthropic => {
                call_anthropic(provider, model, "Reply with OK.", "ping", None)?
            }
        };
        Ok(format!(
            "连接成功 ({model}): {}",
            truncate_utf8(text.trim(), 60)
        ))
    }
}

struct ToolSpec {
    name: String,
    description: String,
    schema: Value,
}

fn build_tool_specs(
    mcp_tools: &[McpTool],
    has_encoder: bool,
) -> (Vec<ToolSpec>, HashMap<String, String>) {
    let mut specs = vec![
        ToolSpec {
            name: "keep".into(),
            description: "新建或更新一条跨回合挂念。".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "concern": { "type": "integer", "description": "要更新的挂念 ID，新建则省略" },
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
        },
        ToolSpec {
            name: "release".into(),
            description: "结束并放下不再需要的挂念。".into(),
            schema: json!({
                "type": "object",
                "properties": { "concern": { "type": "integer" } },
                "required": ["concern"]
            }),
        },
        ToolSpec {
            name: "read".into(),
            description: "按 ID 回读任意历史日志条目的完整内容。".into(),
            schema: json!({
                "type": "object",
                "properties": { "fact": { "type": "integer" } },
                "required": ["fact"]
            }),
        },
    ];
    if has_encoder {
        specs.push(ToolSpec {
            name: "search".into(),
            description: "按语义检索全历史日志。".into(),
            schema: json!({
                "type": "object",
                "properties": { "cue": { "type": "string" } },
                "required": ["cue"]
            }),
        });
    }
    specs.extend([
        ToolSpec {
            name: "say".into(),
            description: "对人说一句话（支持 Markdown）。".into(),
            schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        },
        ToolSpec {
            name: "wake".into(),
            description: "预约未来某个时刻主动唤醒自己。".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "delay_seconds": { "type": "integer", "minimum": 5 },
                    "reason": { "type": "string" }
                },
                "required": ["delay_seconds", "reason"]
            }),
        },
        ToolSpec {
            name: "shell".into(),
            description: "在本地 macOS 上执行 shell 命令（可用于读写文件、编写 MCP Server 等）。".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" }
                },
                "required": ["command"]
            }),
        },
    ]);

    let mut used: HashSet<String> = specs.iter().map(|s| s.name.clone()).collect();
    let mut alias_to_cap = HashMap::new();
    for tool in mcp_tools {
        let alias = safe_tool_name(&tool.capability, &mut used);
        alias_to_cap.insert(alias.clone(), tool.capability.clone());
        specs.push(ToolSpec {
            name: alias,
            description: if tool.description.is_empty() {
                format!("MCP 工具 {}", tool.capability)
            } else {
                tool.description.clone()
            },
            schema: tool.input_schema.clone(),
        });
    }
    (specs, alias_to_cap)
}

fn call_openai(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    user: &str,
    tools: Option<&[ToolSpec]>,
) -> Result<(String, Vec<(String, Value)>), String> {
    let base = provider.base_url.trim().trim_end_matches('/');
    let url = if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    };
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
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
    let msg = resp
        .pointer("/choices/0/message")
        .ok_or_else(|| format!("LLM 响应缺少 choices[0].message: {}", truncate_utf8(&resp.to_string(), 240)))?;
    let text = msg
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut calls = Vec::new();
    if let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        for item in arr {
            let Some(name) = item.pointer("/function/name").and_then(|v| v.as_str()) else {
                continue;
            };
            let raw_args = item.pointer("/function/arguments");
            let args = match raw_args {
                Some(Value::String(s)) => {
                    serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
                }
                Some(v) if v.is_object() => v.clone(),
                _ => json!({}),
            };
            calls.push((name.to_string(), args));
        }
    }
    Ok((text, calls))
}

fn call_anthropic(
    provider: &ProviderConfig,
    model: &str,
    system: &str,
    user: &str,
    tools: Option<&[ToolSpec]>,
) -> Result<(String, Vec<(String, Value)>), String> {
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
        "max_tokens": 4096,
        "system": system,
        "messages": [{ "role": "user", "content": user }]
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
        .ok_or_else(|| format!("Anthropic 响应缺少 content: {}", truncate_utf8(&resp.to_string(), 240)))?;
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
                if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    calls.push((name.to_string(), input));
                }
            }
            _ => {}
        }
    }
    Ok((texts.join("\n"), calls))
}

fn post_json(url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|err| err.to_string())?;
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args(["-sS", "-X", "POST", "--max-time", "90", "-w", "\n%{http_code}"]);
    for (k, v) in headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    cmd.args(["--data-binary", "@-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| format!("启动 curl 失败: {err}"))?;
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
    let parsed: Value = serde_json::from_str(resp_body).unwrap_or_else(|_| json!({ "raw": resp_body }));
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

fn latest_notice_needs_reply(view: &View) -> bool {
    matches!(
        view.recent.first().map(|entry| &entry.fact),
        Some(Fact::Noticed {
            source: Source::Person | Source::Clock,
            ..
        })
    )
}

fn safe_tool_name(capability: &str, used: &mut HashSet<String>) -> String {
    let mut base: String = capability
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
    if base.is_empty() {
        base = "mcp_tool".to_string();
    }
    let mut candidate = base.clone();
    let mut idx = 2usize;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}_{idx}");
        idx += 1;
    }
    candidate
}
