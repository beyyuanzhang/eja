//! Disposable AJA loop. All effects, including model requests, go through the host.
use crate::{Assignment, Result};
pub(crate) mod view;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const FRAME_LIMIT: usize = 256 * 1024;

/// A model request travels wrapped in its JSON envelope, so the transcript budget
/// leaves that much room inside the frame the transport accepts.
pub const FRAME_ENVELOPE: usize = 4 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub timeout_ms: u64,
    pub steps: Option<usize>,
    pub context_bytes: usize,
    pub response_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: 300_000,
            steps: Some(256),
            context_bytes: FRAME_LIMIT - FRAME_ENVELOPE,
            response_bytes: 64 * 1024,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.timeout_ms == 0
            || self.steps == Some(0)
            || self.context_bytes == 0
            || self.context_bytes > FRAME_LIMIT - FRAME_ENVELOPE
            || self.response_bytes == 0
            || self.response_bytes > FRAME_LIMIT / 2
        {
            return Err("invalid AJA limits".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Start {
    pub context_bytes: usize,
    /// The final bounded starting view, prepared by the host before transport.
    pub messages: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    Model {
        messages: Vec<Value>,
    },
    Tool {
        #[serde(default)]
        id: String,
        name: String,
        arguments: Value,
    },
    Finish {
        text: String,
        reason: String,
    },
}

pub(crate) fn send(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > FRAME_LIMIT {
        return Err("AJA frame exceeds limit".into());
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn receive<T: serde::de::DeserializeOwned>(reader: &mut impl BufRead) -> Result<T> {
    let mut bytes = Vec::new();
    // Reading one byte beyond the cap detects an oversized frame without allocating it.
    let n = std::io::Read::take(reader, FRAME_LIMIT as u64 + 2).read_until(b'\n', &mut bytes)?;
    if n == 0 {
        return Err("AJA host disconnected".into());
    }
    if bytes.len() > FRAME_LIMIT + 1 || bytes.last() != Some(&b'\n') {
        return Err("invalid or oversized AJA frame".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
fn transcript_bytes(messages: &[Value]) -> usize {
    view::size(messages)
}
#[cfg(test)]
fn compact(messages: &mut Vec<Value>, budget: usize) {
    view::fit(messages, budget);
}

pub(crate) fn fresh_messages(assignment: &Assignment, limits: &Limits) -> Vec<Value> {
    let budget = limits.steps.map_or_else(
        || format!("You have {} seconds.", limits.timeout_ms / 1000),
        |steps| {
            format!(
                "You have at most {steps} model turns and {} seconds.",
                limits.timeout_ms / 1000
            )
        },
    );
    let system = format!("You are AJA, one independent work node. Complete the assigned work using supplied tools. Your workspace is the current project; inherited messages describe earlier observations, not a restored world. {budget} Acceptance is the contract. Stop once it holds and report what you did. Tool results carry record addresses; use eja_read_record to read omitted passages. Never claim a failed tool succeeded.");
    let mut brief = assignment.objective.clone();
    if !assignment.input.trim().is_empty() && assignment.input.trim() != brief.trim() {
        brief.push_str("\n\nTask as given:\n");
        brief.push_str(&assignment.input);
    }
    if !assignment.acceptance.trim().is_empty() {
        brief.push_str("\n\nAcceptance: ");
        brief.push_str(&assignment.acceptance);
    }
    if assignment.resume.is_some() {
        brief = format!("Continuing as a new node. {brief}");
    }
    vec![
        json!({"role":"system","content":system}),
        json!({"role":"user","content":brief}),
    ]
}

#[derive(Debug)]
pub(crate) struct ContextBudget;
impl std::fmt::Display for ContextBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AJA context budget exhausted: assignment does not fit")
    }
}
impl std::error::Error for ContextBudget {}

pub(crate) fn prepare_messages(
    assignment: &Assignment,
    limits: &Limits,
    inherited: Vec<Value>,
) -> Result<Vec<Value>> {
    let fresh = fresh_messages(assignment, limits);
    let mut messages = vec![fresh[0].clone()];
    messages.extend(
        inherited
            .into_iter()
            .filter(|message| message["role"] != "system"),
    );
    messages.push(fresh[1].clone());
    if !view::fit(&mut messages, limits.context_bytes) {
        return Err(ContextBudget.into());
    }
    Ok(messages)
}

/// Every model adapter and IPC consumer shares the same assistant contract.
pub(crate) fn validate_assistant(message: &Value) -> Result<()> {
    if !message.is_object() || message["role"] != "assistant" {
        return Err("model message must be an assistant object".into());
    }
    let mut ids = std::collections::HashSet::new();
    if let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) {
        for call in calls.as_array().ok_or("invalid tool_calls")? {
            let id = call["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or("invalid tool id")?;
            if !ids.insert(id)
                || call["type"] != "function"
                || call["function"]["name"].as_str().is_none_or(str::is_empty)
            {
                return Err("invalid or duplicate tool call".into());
            }
            let arguments: Value = serde_json::from_str(
                call["function"]["arguments"]
                    .as_str()
                    .ok_or("missing tool arguments")?,
            )?;
            if !arguments.is_object() {
                return Err("tool arguments must be an object".into());
            }
        }
    }
    if ids.is_empty() && !message.get("content").is_some_and(Value::is_string) {
        return Err("a final assistant message needs text content".into());
    }
    Ok(())
}

/// A spent budget is a stopping point, not a failed piece of work.
fn stopped(reason: &str) -> (String, String) {
    (format!("{reason}; continue in a new node"), "budget".into())
}

fn request(reader: &mut impl BufRead, writer: &mut impl Write, request: Request) -> Result<Value> {
    send(writer, &request)?;
    receive(reader)
}

fn run(
    start: Start,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(String, String)> {
    if start.context_bytes == 0 || start.context_bytes > FRAME_LIMIT - FRAME_ENVELOPE {
        return Err("invalid AJA context limit".into());
    }
    let mut messages = start.messages;
    loop {
        // The transcript is a working view, not the record: the host keeps every
        // tool result in context.jsonl, so the oldest results can be elided in
        // place once the view outgrows its declared budget.
        if !view::fit(&mut messages, start.context_bytes) {
            return Ok(stopped("AJA context budget exhausted"));
        }
        let message = request(
            reader,
            writer,
            Request::Model {
                messages: messages.clone(),
            },
        )?;
        validate_assistant(&message)?;
        let calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if calls.is_empty() {
            // The node's last message is text, never a schema: EJA reads it as
            // the node's account and judges it with the record of its calls.
            let text = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Ok((text, "message".into()));
        }
        messages.push(message);
        for call in calls {
            let name = call["function"]["name"]
                .as_str()
                .ok_or("missing tool name")?;
            let arguments = serde_json::from_str(
                call["function"]["arguments"]
                    .as_str()
                    .ok_or("missing arguments")?,
            )?;
            let result = request(
                reader,
                writer,
                Request::Tool {
                    id: call["id"].as_str().unwrap_or_default().into(),
                    name: name.into(),
                    arguments,
                },
            )?;
            messages.push(
                json!({"role":"tool","tool_call_id":call["id"],"content":result.to_string()}),
            );
        }
    }
}

/// Private worker mode of the same binary. It receives no credentials or state-log path.
pub fn worker_main() -> Result<()> {
    let mut reader = std::io::BufReader::new(std::io::stdin().lock());
    let mut writer = std::io::stdout().lock();
    let start: Start = receive(&mut reader)?;
    let (text, reason) =
        run(start, &mut reader, &mut writer).unwrap_or_else(|e| (e.to_string(), "error".into()));
    send(&mut writer, &Request::Finish { text, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_accepts_the_exact_payload_cap_and_rejects_oversize_or_torn_frames() {
        let value = Value::String("x".repeat(FRAME_LIMIT - 2));
        let mut bytes = Vec::new();
        send(&mut bytes, &value).unwrap();
        assert_eq!(receive::<Value>(&mut bytes.as_slice()).unwrap(), value);
        assert!(receive::<Value>(&mut &bytes[..bytes.len() - 1]).is_err());
        assert!(send(&mut Vec::new(), &Value::String("x".repeat(FRAME_LIMIT - 1))).is_err());
        let mut oversized = vec![b' '; FRAME_LIMIT + 1];
        oversized.push(b'\n');
        assert!(receive::<Value>(&mut oversized.as_slice()).is_err());
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    fn tool_message(id: &str, payload: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": payload})
    }

    fn assistant_call(id: &str, name: &str) -> Value {
        json!({"role": "assistant", "content": null,
               "tool_calls": [{"id": id, "type": "function",
                               "function": {"name": name, "arguments": "{}"}}]})
    }

    #[test]
    fn a_transcript_over_budget_elides_the_oldest_results_and_keeps_the_recent_ones() {
        let mut messages = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "u"}),
        ];
        for round in 0..12 {
            let id = format!("call_{round}");
            messages.push(assistant_call(&id, "read_file"));
            messages.push(tool_message(&id, &"x".repeat(2000)));
        }
        let before = transcript_bytes(&messages);
        // Eight recent results stay verbatim, so the budget must be able to hold them.
        compact(&mut messages, 20_000);
        let after = transcript_bytes(&messages);
        assert!(after < before, "{after} !< {before}");
        let tool_contents: Vec<&str> = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert_eq!(tool_contents.len(), 12, "no message is removed");
        assert_eq!(
            serde_json::from_str::<Value>(tool_contents[0]).unwrap()["elided"],
            true
        );
        assert!(tool_contents.iter().any(|c| c.len() == 2000));
        assert!(after <= 20_000, "{after}");
    }

    #[test]
    fn a_transcript_inside_budget_is_left_alone() {
        let mut messages = vec![json!({"role": "system", "content": "s"})];
        messages.push(assistant_call("call_1", "read_file"));
        messages.push(tool_message("call_1", &"x".repeat(100)));
        let before = messages.clone();
        compact(&mut messages, 10_000);
        assert_eq!(messages, before);
    }
}
