//! Disposable AJA loop. All effects, including model requests, go through the host.
use crate::{Assignment, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const FRAME_LIMIT: usize = 256 * 1024;

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
            steps: None,
            context_bytes: FRAME_LIMIT,
            response_bytes: 64 * 1024,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.timeout_ms == 0
            || self.steps == Some(0)
            || self.context_bytes == 0
            || self.context_bytes > FRAME_LIMIT
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
    pub assignment: Assignment,
    pub limits: Limits,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    Model { messages: Vec<Value> },
    Tool { name: String, arguments: Value },
    Finish { text: String, reason: String },
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

/// How many recent tool results stay verbatim when the transcript compacts.
const KEEP_RECENT_TOOL_RESULTS: usize = 8;

fn transcript_bytes(messages: &[Value]) -> usize {
    serde_json::to_vec(messages)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

/// Elide the oldest tool results until the transcript fits its budget. The tool
/// call and its result message stay in place, so the provider's turn structure
/// is untouched; only the payload is replaced by an accounted-for marker.
fn compact(messages: &mut [Value], budget: usize) {
    if transcript_bytes(messages) <= budget {
        return;
    }
    let mut names = std::collections::HashMap::new();
    for message in messages.iter() {
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let (Some(id), Some(name)) = (
                    call.get("id").and_then(Value::as_str),
                    call.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str),
                ) {
                    names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    let tools: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(Value::as_str) == Some("tool"))
        .map(|(index, _)| index)
        .collect();
    let elidable = tools.len().saturating_sub(KEEP_RECENT_TOOL_RESULTS);
    for &index in tools.iter().take(elidable) {
        let id = messages[index]
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = names.get(id).map(String::as_str).unwrap_or("tool");
        let bytes = messages[index]
            .get("content")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        messages[index]["content"] = json!(format!(
            "<elided {bytes} bytes from {name}; the full result stays in the record>"
        ));
        if transcript_bytes(messages) <= budget {
            return;
        }
    }
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
    start.limits.validate()?;
    let budget = match start.limits.steps {
        Some(steps) => format!(
            "You have at most {steps} model turns and {} seconds.",
            start.limits.timeout_ms / 1000
        ),
        None => format!("You have {} seconds.", start.limits.timeout_ms / 1000),
    };
    let system = format!(
        "You are AJA, one independent work node. Complete only the assigned work using the supplied tools. Your work area is scratch: what you write there is sealed as your artifacts, which are evidence for EJA, not the deliverable. The project is the shared result: when the work must land there, change it with a tool that reaches the project, at the exact paths the work requires. {budget} The Acceptance line is the contract: the work is done when it holds. Stop calling tools then; your last message is read by EJA as your account of what you did, alongside the record of your calls. Never claim a tool succeeded when it failed.",
    );
    let mut brief = start.assignment.objective.clone();
    if !start.assignment.acceptance.trim().is_empty() {
        brief.push_str("\n\nAcceptance: ");
        brief.push_str(start.assignment.acceptance.trim());
    }
    let mut messages = vec![
        json!({"role":"system","content":system}),
        json!({"role":"user","content":brief}),
    ];
    let mut turns = 0usize;
    loop {
        if let Some(limit) = start.limits.steps {
            if turns >= limit {
                return Ok(stopped(&format!(
                    "AJA step budget of {limit} turns exhausted"
                )));
            }
        }
        turns += 1;
        // The transcript is a working view, not the record: the host keeps every
        // tool result in context.jsonl, so the oldest results can be elided in
        // place once the view outgrows its declared budget.
        compact(&mut messages, start.limits.context_bytes);
        if serde_json::to_vec(&messages)?.len() > start.limits.context_bytes {
            return Ok(stopped("AJA context budget exhausted"));
        }
        let message = request(
            reader,
            writer,
            Request::Model {
                messages: messages.clone(),
            },
        )?;
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
        assert!(tool_contents[0].starts_with("<elided 2000 bytes from read_file"));
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
