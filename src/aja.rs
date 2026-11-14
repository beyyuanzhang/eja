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
    messages
        .iter()
        .map(|message| message.to_string().len())
        .sum()
}
/// Fold a live transcript to its budget. The head (system and assignment) and
/// the newest steps stay whole; older steps become one address line each; the
/// oldest fold into a single address. Returns false when even that does not fit,
/// which is the node's stopping point, not a licence to keep re-sending at the cap.
pub(crate) fn fold_transcript(messages: &mut Vec<Value>, budget: usize) -> bool {
    let head = messages
        .iter()
        .take_while(|message| message["role"] == "system" || message["role"] == "user")
        .count()
        .min(2);
    if messages.len() <= head {
        // Nothing to fold: the assignment itself must fit.
        return messages
            .iter()
            .map(|message| message.to_string().len())
            .sum::<usize>()
            + 8
            <= budget;
    }
    // A step is one assistant message with the tool results it asked for.
    let mut steps: Vec<(usize, usize)> = Vec::new();
    let mut index = head;
    while index < messages.len() {
        let start = index;
        index += 1;
        while index < messages.len() && messages[index]["role"] == "tool" {
            index += 1;
        }
        if start < index {
            steps.push((start, index));
        }
    }
    if steps.is_empty() {
        return true;
    }
    let bodies: Vec<Vec<Value>> = steps
        .iter()
        .map(|(start, end)| messages[*start..*end].to_vec())
        .collect();
    let regions: Vec<crate::fold::Region> = steps
        .iter()
        .zip(&bodies)
        .enumerate()
        .rev()
        .map(|(position, ((start, end), body))| {
            let bytes = body.iter().map(|m| m.to_string().len()).sum::<usize>();
            crate::fold::Region::new(
                format!("step:{}", position + 1),
                step_line(*start, *end, &messages[*start..*end]),
                bytes,
            )
        })
        .collect();
    let head_bytes: usize = messages[..head].iter().map(|m| m.to_string().len()).sum();
    let view = crate::fold::assemble(&regions, budget.saturating_sub(head_bytes + 8));
    let kept: Vec<usize> = view.record.iter().map(|i| steps.len() - 1 - *i).collect();
    let tail = crate::fold::tail(&view, "Earlier steps of this node, folded:");
    let mut rebuilt: Vec<Value> = messages[..head].to_vec();
    if let Some(text) = &tail {
        rebuilt.push(json!({"role":"user","content":text}));
    }
    let mut ordered = kept;
    ordered.sort_unstable();
    for step in &ordered {
        let (start, end) = steps[*step];
        rebuilt.extend_from_slice(&messages[start..end]);
    }
    *messages = rebuilt;
    let size: usize = messages.iter().map(|m| m.to_string().len()).sum();
    size <= budget
}

/// One line that stands for a step, naming the record entry that unfolds it.
fn step_line(start: usize, end: usize, messages: &[Value]) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut address = String::new();
    if let Some(calls) = messages[0]["tool_calls"].as_array() {
        for call in calls {
            names.push(
                call["function"]["name"]
                    .as_str()
                    .unwrap_or("tool")
                    .to_string(),
            );
        }
    }
    let mut snippet = String::new();
    for message in messages.iter().skip(1) {
        let content = message["content"].as_str().unwrap_or("");
        if address.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(content) {
                if let Some(record) = value.get("record") {
                    address = format!(
                        "{}/{}",
                        record["node"].as_u64().unwrap_or(0),
                        record["entry"].as_u64().unwrap_or(0)
                    );
                }
            }
        }
        if snippet.is_empty() {
            snippet = content.replace(['\n', '\r'], " ");
            snippet.truncate(80);
        }
    }
    let _ = start;
    let _ = end;
    let action = if names.is_empty() {
        "said".to_string()
    } else {
        names.join("+")
    };
    let suffix = if address.is_empty() {
        String::new()
    } else {
        format!(" (record {address})")
    };
    format!("  step: {action}{suffix} → {snippet}")
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
    if !fold_transcript(&mut messages, limits.context_bytes) {
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
        if !fold_transcript(&mut messages, start.context_bytes) {
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
mod fold_tests {
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
    fn an_over_budget_transcript_folds_older_steps_into_addresses() {
        let mut messages = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": "u"}),
        ];
        for round in 0..12 {
            let id = format!("call_{round}");
            messages.push(assistant_call(&id, "read_file"));
            messages.push(tool_message(
                &id,
                &json!({"record": {"node": 1, "entry": round}, "result": "x".repeat(2000)})
                    .to_string(),
            ));
        }
        let before = transcript_bytes(&messages);
        assert!(fold_transcript(&mut messages, 20_000));
        let after = transcript_bytes(&messages);
        assert!(after < before, "{after} !< {before}");
        assert!(after <= 20_000, "{after}");
        // The folded part is one message that names the record addresses.
        let folded = messages
            .iter()
            .find(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Earlier steps of this node, folded:"))
            })
            .expect("a folded map");
        assert!(folded["content"].as_str().unwrap().contains("record 1/"));
        // Newest steps stay whole: their tool payloads are still verbatim.
        assert!(messages.iter().any(|message| message["content"]
            .as_str()
            .is_some_and(|text| text.len() > 2000)));
        // Every step is still addressable: none is silently dropped.
        assert!(folded["content"]
            .as_str()
            .unwrap()
            .contains("unfold by handle"));
    }

    #[test]
    fn a_transcript_inside_budget_is_left_alone() {
        let mut messages = vec![json!({"role": "system", "content": "s"})];
        messages.push(json!({"role": "user", "content": "u"}));
        messages.push(assistant_call("call_1", "read_file"));
        messages.push(tool_message("call_1", &"x".repeat(100)));
        let before = messages.clone();
        assert!(fold_transcript(&mut messages, 10_000));
        assert_eq!(messages, before);
    }
}
