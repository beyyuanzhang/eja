use super::Retriever;
use crate::{format_utc, Event, Result};
use serde_json::Value;
use std::time::Duration;

/// A retriever call may not outlive this; it is a host resource, not a result cap.
const RETRIEVE_TIMEOUT: Duration = Duration::from_secs(120);

use super::{cut_chars, label, llm_env, Control, OpenAiModel};

pub struct ModelRetriever {
    model: OpenAiModel,
    budget: usize,
}

impl ModelRetriever {
    pub fn from_env() -> Option<Self> {
        let (base, key, model) = llm_env()?;
        Some(Self::new(
            base,
            key,
            std::env::var("LNA_RETRIEVER_MODEL").unwrap_or(model),
        ))
    }
    pub fn new(base: impl Into<String>, key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            model: OpenAiModel::new(base, key, model),
            budget: budget(),
        }
    }

    pub fn with_budget(mut self, budget: usize) -> Self {
        self.budget = budget;
        self
    }

    /// Fill newest-first until the declared budget; the cut is stated in the prompt.
    fn index(events: &[Event], budget: usize) -> String {
        let mut newest_first = Vec::new();
        let mut used = 0usize;
        for event in events.iter().rev() {
            let line = format!(
                "#{} [{}] {} scope={} {}",
                event.id,
                label(event.kind),
                format_utc(event.at),
                event.scope,
                event.text
            );
            let need = line.len() + 1;
            if used + need > budget {
                let left = budget.saturating_sub(used);
                if left > 8 {
                    let cut = cut_chars(&line, left.saturating_sub(4));
                    newest_first.push(format!("{cut}…"));
                }
                break;
            }
            used += need;
            newest_first.push(line);
        }
        newest_first.reverse();
        newest_first.join("\n")
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
        out
    }
}

impl Retriever for ModelRetriever {
    fn retrieve(&self, query: &str, events: &[Event]) -> Result<Vec<u64>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let index = Self::index(events, self.budget);
        let shown = index.lines().count();
        let coverage = if shown < events.len() {
            format!(
                "SHOWING {shown} OF {} EVENTS (newest fit the budget; older events are not shown)",
                events.len()
            )
        } else {
            format!("SHOWING ALL {} EVENTS", events.len())
        };
        let prompt = format!(
            "Relevant event ids for the query. Return ONLY JSON {{\"ids\":[1,2]}}.\n\n{coverage}\n\nQUERY:\n{query}\n\nEVENTS:\n{index}"
        );
        let messages = vec![
            serde_json::json!({"role":"system","content":"Select relevant event ids from the supplied events. Return ONLY JSON {\"ids\":[1,2]}. Never invent ids."}),
            serde_json::json!({"role":"user","content":prompt}),
        ];
        let control = Control::new(
            RETRIEVE_TIMEOUT,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let response = self
            .model
            .request(&messages, &[], crate::aja::FRAME_LIMIT, &control)?;
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

fn budget() -> usize {
    std::env::var("EJA_RETRIEVER_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(crate::aja::FRAME_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kind;

    fn event(id: u64, text: &str) -> Event {
        Event {
            id,
            at: 1,
            turn: 1,
            scope: "task:1".into(),
            kind: Kind::RecordDecision,
            text: text.into(),
            closes: None,
            data: None,
        }
    }

    #[test]
    fn the_index_fills_the_declared_budget_and_keeps_text_intact() {
        let events: Vec<Event> = (1..=50)
            .map(|id| {
                event(
                    id,
                    &format!("event {id} with a long body {}", "x".repeat(200)),
                )
            })
            .collect();
        let roomy = ModelRetriever::index(&events, 10_000_000);
        assert_eq!(roomy.lines().count(), 50);
        assert!(roomy.contains("event 1 with a long body"));
        assert!(roomy.contains(&"x".repeat(200)));
        let tight = ModelRetriever::index(&events, 2_000);
        let shown = tight.lines().count();
        assert!(shown < 50, "a small budget shows fewer events, got {shown}");
        assert!(tight.contains("event 50"), "the newest events fit first");
        assert!(!tight.contains("event 1 with"), "the oldest are omitted");
    }

    #[test]
    fn a_long_event_is_cut_at_the_budget_not_at_a_fixed_width() {
        let events = vec![event(1, &"y".repeat(1_000))];
        let short = ModelRetriever::index(&events, 300);
        let cut = short.lines().next().unwrap();
        assert!(cut.len() <= 300, "{}", cut.len());
        assert!(cut.ends_with('…'));
        let full = ModelRetriever::index(&events, 10_000);
        assert!(full.len() > 1_000);
    }
}
