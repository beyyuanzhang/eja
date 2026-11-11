//! Host-owned model transport shared by EJA and its AJA nodes.
use crate::{aja::Limits, Result};
use serde_json::{json, Value};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Control {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Control {
    pub fn new(timeout: Duration, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled,
        }
    }
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err("AJA cancelled".into());
        }
        if Instant::now() >= self.deadline {
            return Err("AJA timed out".into());
        }
        Ok(())
    }
    pub fn remaining(&self) -> Result<Duration> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
    fn wait<F: Future<Output = Result<Value>>>(&self, future: F) -> Result<Value> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            tokio::pin!(future);
            loop {
                tokio::select! {
                    result = &mut future => return result,
                    _ = tokio::time::sleep(Duration::from_millis(10)) => self.check()?,
                }
            }
        })
    }
}

pub trait Model: Send + Sync {
    /// Implementations must respect the host's cancellation/deadline and response cap.
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        limits: &Limits,
        control: &Control,
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
    ) -> Result<Value> {
        let timeout = control.remaining()?;
        let mut body = json!({"model":self.model,"messages":messages,"temperature":0.2,"response_format":{"type":"json_object"}});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        control.wait(async {
            let client = reqwest::Client::builder().timeout(timeout).build()?;
            let mut response = client
                .post(format!(
                    "{}/chat/completions",
                    self.base.trim_end_matches('/')
                ))
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if chunk.len() > cap.saturating_sub(bytes.len()) {
                    return Err("model response limit reached".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(serde_json::from_slice(&bytes)?)
        })
    }
}

impl Model for OpenAiModel {
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        limits: &Limits,
        control: &Control,
    ) -> Result<Value> {
        let value = self.request(messages, tools, limits.response_bytes, control)?;
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
        let mut ids = std::collections::HashSet::new();
        if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
            for call in calls.as_array().ok_or("invalid tool_calls")? {
                let id = call["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("invalid tool id")?;
                if !ids.insert(id)
                    || call["type"] != "function"
                    || call["function"]["name"].as_str().is_none_or(str::is_empty)
                {
                    return Err("invalid or duplicate tool call".into());
                }
                let args: Value = serde_json::from_str(
                    call["function"]["arguments"]
                        .as_str()
                        .ok_or("missing tool arguments")?,
                )?;
                if !args.is_object() {
                    return Err("tool arguments must be an object".into());
                }
            }
        }
        if (reason == "tool_calls") == ids.is_empty() {
            return Err("inconsistent tool finish reason".into());
        }
        Ok(message)
    }
}
