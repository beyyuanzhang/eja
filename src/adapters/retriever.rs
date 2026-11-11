use crate::{format_utc, Event, Result, Retriever};
use serde_json::Value;
use std::time::Duration;

use super::{label, llm_env};

pub struct ModelRetriever {
    base: String,
    key: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl ModelRetriever {
    pub fn from_env() -> Option<Self> {
        let (base, key, model) = llm_env()?;
        let model = std::env::var("LNA_RETRIEVER_MODEL").unwrap_or(model);
        Some(Self::new(base, key, model))
    }

    pub fn new(base: impl Into<String>, key: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("build retriever client");
        Self {
            base: base.into(),
            key: key.into(),
            model: model.into(),
            client,
        }
    }

    fn index(events: &[Event]) -> String {
        let mut lines = Vec::new();
        for event in events.iter().rev().take(1000).rev() {
            let text: String = event.text.chars().take(300).collect();
            lines.push(format!(
                "#{} [{}] {} scope={} {}",
                event.id,
                label(event.kind),
                format_utc(event.at),
                event.scope,
                text
            ));
        }
        lines.join("\n")
    }

    fn expand(ids: &[u64], events: &[Event]) -> Vec<u64> {
        let mut out: Vec<u64> = ids.to_vec();
        for id in ids {
            if let Some(event) = events.iter().find(|event| event.id == *id) {
                if let Some(target) = event.closes {
                    out.push(target);
                }
            }
            if let Some(event) = events.iter().find(|event| event.id == *id) {
                if event.scope.starts_with("task:") {
                    for related in events
                        .iter()
                        .filter(|other| other.scope == event.scope)
                        .map(|other| other.id)
                    {
                        out.push(related);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out.truncate(16);
        out
    }
}

impl Retriever for ModelRetriever {
    fn retrieve(&self, query: &str, events: &[Event]) -> Result<Vec<u64>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let prompt = format!(
            "Relevant event ids for the query. Return ONLY JSON {{\"ids\":[1,2]}}.\n\nQUERY:\n{query}\n\nEVENTS:\n{}",
            Self::index(events)
        );
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": "You are a retrieval function. Select only ids from the supplied events. Never invent ids. Return JSON only."},
                {"role": "user", "content": prompt}
            ],
            "temperature": 0.0,
        });
        let response = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base.trim_end_matches('/')
            ))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.json::<Value>())?;
        let raw = response
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        let ids = parse_ids(raw, events);
        Ok(Self::expand(&ids, events))
    }
}

fn parse_ids(raw: &str, events: &[Event]) -> Vec<u64> {
    let valid: std::collections::HashSet<u64> = events.iter().map(|event| event.id).collect();
    let start = raw.find('{');
    let end = raw.rfind('}');
    let candidate = match (start, end) {
        (Some(start), Some(end)) if end > start => &raw[start..=end],
        _ => raw,
    };
    let mut ids = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(candidate) {
        if let Some(array) = value.get("ids").and_then(Value::as_array) {
            for id in array.iter().filter_map(Value::as_u64) {
                if valid.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}
