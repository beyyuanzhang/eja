//! Disposable AJA loop. All effects, including model requests, go through the host.
use crate::{Assignment, Report, ReportStatus, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const FRAME_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub timeout_ms: u64,
    pub steps: usize,
    pub context_bytes: usize,
    pub response_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: 300_000,
            steps: 16,
            context_bytes: FRAME_LIMIT,
            response_bytes: 64 * 1024,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.timeout_ms == 0
            || self.steps == 0
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
    Finish { report: Report },
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

/// A spent budget is a stopping point, not a failed piece of work.
fn budget_exhausted(reason: &str) -> Report {
    Report {
        status: ReportStatus::Blocked,
        summary: format!("{reason}; continue in a new node"),
        artifacts: Vec::new(),
        evidence: Vec::new(),
        open_questions: vec!["continue in a new node".to_string()],
        tools: Vec::new(),
    }
}

fn request(reader: &mut impl BufRead, writer: &mut impl Write, request: Request) -> Result<Value> {
    send(writer, &request)?;
    receive(reader)
}

fn run(start: Start, reader: &mut impl BufRead, writer: &mut impl Write) -> Result<Report> {
    start.limits.validate()?;
    let system = format!(
        "You are AJA, one independent work node. Complete only the assigned work using the supplied tools. Paths are relative to your work area unless a tool explicitly reads the project. You have at most {} model turns and {} seconds: budget them, and spend the last turn writing the report instead of calling tools. Return a final JSON report: {{\"status\":\"done|blocked|failed\",\"summary\":\"...\",\"artifacts\":[\"relative work file\"],\"evidence\":[],\"open_questions\":[]}}. A done report is a claim for EJA to inspect. Never claim a tool succeeded when it failed.",
        start.limits.steps,
        start.limits.timeout_ms / 1000,
    );
    let mut messages = vec![
        json!({"role":"system","content":system}),
        json!({"role":"user","content":start.assignment.objective}),
    ];
    for _ in 0..start.limits.steps {
        if serde_json::to_vec(&messages)?.len() > start.limits.context_bytes {
            return Ok(budget_exhausted("AJA context budget exhausted"));
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
            let text = message
                .get("content")
                .and_then(Value::as_str)
                .ok_or("missing AJA report")?;
            let report: Report = serde_json::from_str(text)?;
            if report.summary.trim().is_empty() {
                return Err("empty AJA report".into());
            }
            return Ok(report);
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
    Ok(budget_exhausted(&format!(
        "AJA step budget of {} turns exhausted",
        start.limits.steps
    )))
}

/// Private worker mode of the same binary. It receives no credentials or state-log path.
pub fn worker_main() -> Result<()> {
    let mut reader = std::io::BufReader::new(std::io::stdin().lock());
    let mut writer = std::io::stdout().lock();
    let start: Start = receive(&mut reader)?;
    let report =
        run(start, &mut reader, &mut writer).unwrap_or_else(|e| Report::failed(e.to_string()));
    send(&mut writer, &Request::Finish { report })
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
