//! Host-owned model transport shared by EJA and its AJA ajas.
use crate::{aja::Limits, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub use crate::Control;

/// Attempts per model call, including the first one.
const MODEL_ATTEMPTS: usize = 4;
const MODEL_RETRY_BASE_MS: u64 = 250;

fn retry_delay(attempt: usize) -> Duration {
    let factor = 1u64 << (attempt - 1).min(5);
    Duration::from_millis((MODEL_RETRY_BASE_MS * factor).min(8_000))
}

pub trait Model: Send + Sync {
    /// Implementations must respect the host's cancellation/deadline and response cap.
    /// `json_mode` asks the provider for a JSON object. A caller that acts
    /// through tool calls does not need it, and a provider may reject it when
    /// the prompt never mentions JSON.
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        limits: &Limits,
        control: &Control,
        json_mode: bool,
    ) -> Result<Value>;
}

pub struct OpenAiModel {
    base: String,
    key: String,
    model: String,
}

impl OpenAiModel {
    pub fn new(base: impl Into<String>, key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            key: key.into(),
            model: model.into(),
        }
    }
    pub fn from_env() -> Option<Self> {
        let (base, key, model) = super::llm_env()?;
        Some(Self::new(
            base,
            key,
            std::env::var("EJA_AJA_MODEL").unwrap_or(model),
        ))
    }
    pub(crate) fn request(
        &self,
        messages: &[Value],
        tools: &[Value],
        cap: usize,
        control: &Control,
        json_mode: bool,
    ) -> Result<Value> {
        let timeout = control.remaining()?;
        let mut body = json!({"model":self.model,"messages":messages,"temperature":0.2});
        if json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(super::controlled(control, async {
            let client = reqwest::Client::builder().timeout(timeout).build()?;
            let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
            // A provider hiccup (429, 5xx, a dropped connection) is not EJA's
            // error. Retry a few times with backoff; the run deadline still
            // bounds the whole call.
            let mut attempt = 0;
            loop {
                attempt += 1;
                let sent = client
                    .post(&url)
                    .bearer_auth(&self.key)
                    .json(&body)
                    .send()
                    .await;
                let mut response = match sent {
                    Ok(response) => response,
                    Err(error) => {
                        if attempt < MODEL_ATTEMPTS {
                            tokio::time::sleep(retry_delay(attempt)).await;
                            continue;
                        }
                        return Err(error.into());
                    }
                };
                let status = response.status();
                if !status.is_success() {
                    let transient = status.as_u16() == 429 || status.is_server_error();
                    if transient && attempt < MODEL_ATTEMPTS {
                        tokio::time::sleep(retry_delay(attempt)).await;
                        continue;
                    }
                    return Err(format!("model request failed: HTTP {status}").into());
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await? {
                    if chunk.len() > cap.saturating_sub(bytes.len()) {
                        return Err("model response limit reached".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                return Ok(serde_json::from_slice(&bytes)?);
            }
        }))
    }
}

impl Model for OpenAiModel {
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        limits: &Limits,
        control: &Control,
        json_mode: bool,
    ) -> Result<Value> {
        let value = self.request(messages, tools, limits.response_bytes, control, json_mode)?;
        let reason = value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .ok_or("missing finish_reason")?;
        if !matches!(reason, "stop" | "tool_calls") {
            return Err(format!("incomplete model response: {reason}").into());
        }
        let mut message = value
            .pointer("/choices/0/message")
            .cloned()
            .ok_or("missing model message")?;
        if !message.is_object() {
            return Err("model message must be an object".into());
        }
        message["role"] = json!("assistant");
        crate::aja::validate_assistant(&message)?;
        let has_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if (reason == "tool_calls") != has_calls {
            return Err("inconsistent tool finish reason".into());
        }
        Ok(message)
    }
}
