use crate::{format_utc, Brain, Decision, EjaContext};
use serde_json::{json, Value};
use std::time::Duration;

use super::{label, llm_env, status};

fn relation(kind: crate::RelationKind) -> &'static str {
    match kind {
        crate::RelationKind::PartOf => "part_of",
        crate::RelationKind::DependsOn => "depends_on",
        crate::RelationKind::Continues => "continues",
        crate::RelationKind::RelatesTo => "relates_to",
        crate::RelationKind::Supersedes => "supersedes",
        crate::RelationKind::Contradicts => "contradicts",
    }
}

pub const SYSTEM_PROMPT: &str = "\
You are EJA, one continuous subject. You own identity, commitments and the \
task index; you do not execute work. AJA sessions execute one assignment each and \
return a report. A report is a claim, not a verified outcome: only close a task \
when the report satisfies its acceptance criteria, otherwise reject it or ask \
the user. EJA has no execution tools and no fallback executor: never offer to do \
the work yourself. A correction targets an explicit event id; never invent one. Identity changes \
(goal, commitment, correction) are proposals: propose them, then confirm only \
after the user has confirmed. Never confirm a proposal in the same turn it was \
created. \
The current input is separate from the persistent goal. Use a goal/commitment \
proposal without target_id to add one; with target_id to replace an active event \
of the same kind. Use correction with target_id only to withdraw that event. \
The proposal text is the replacement content, or the withdrawal reason. \
Actions execute in their listed order. Reading a task, recalling history, reading \
an artifact or dispatching an AJA returns observations for your next decision. \
Inspect those observations before deciding acceptance. To finish, return your \
final reply with no observation-producing actions. \
Return ONLY a JSON object: {\"reply\":\"...\",\"actions\":[...]}. Action shapes: \
{\"op\":\"propose\",\"kind\":\"goal|commitment|correction\",\"target_id\":1,\"text\":\"...\"}; \
{\"op\":\"confirm\",\"proposal_id\":1}; \
{\"op\":\"reject\",\"proposal_id\":1,\"note\":\"...\"}; \
{\"op\":\"decision|relationship\",\"text\":\"...\"}; \
{\"op\":\"open_task\",\"title\":\"...\",\"request\":\"...\",\"acceptance\":\"...\",\"run\":true}; \
{\"op\":\"dispatch\",\"task_id\":1}; \
{\"op\":\"close_task\",\"task_id\":1,\"accepted\":true,\"note\":\"...\"}; \
{\"op\":\"read_task\",\"task_id\":1}; \
{\"op\":\"recall\",\"query\":\"what did we decide about X?\"}; \
{\"op\":\"read_artifact\",\"path\":\"out.md\"}.";

pub fn render_context(context: &EjaContext) -> String {
    let mut lines = vec![
        format!("NOW (UTC): {}", format_utc(context.now)),
        String::new(),
        "CURRENT GOAL:".to_string(),
        context.goal.as_ref().map_or_else(
            || "  (none)".to_string(),
            |goal| format!("  #{} {}", goal.id, goal.text),
        ),
        String::new(),
        "ACTIVE IDENTITY:".to_string(),
    ];
    for event in &context.identity {
        lines.push(format!(
            "  #{} [{}] {}: {}",
            event.id,
            label(event.kind),
            format_utc(event.at),
            event.text
        ));
    }
    if context.identity.is_empty() {
        lines.push("  (none)".to_string());
    }
    lines.push(String::new());
    lines.push("OPEN COMMITMENTS:".to_string());
    for event in &context.commitments {
        lines.push(format!(
            "  #{} {}: {}",
            event.id,
            format_utc(event.at),
            event.text
        ));
    }
    if context.commitments.is_empty() {
        lines.push("  (none)".to_string());
    }
    lines.push(String::new());
    lines.push("PENDING PROPOSALS:".to_string());
    for proposal in &context.proposals {
        lines.push(format!(
            "  proposal #{} [{}] target={} turn={}: {}",
            proposal.id,
            format!("{:?}", proposal.kind).to_lowercase(),
            proposal
                .target_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            proposal.turn,
            proposal.text
        ));
    }
    if context.proposals.is_empty() {
        lines.push("  (none)".to_string());
    }
    lines.push(String::new());
    lines.push(format!("TASK INDEX ({}):", context.tasks.len()));
    for task in &context.tasks {
        lines.push(format!(
            "  #{} [{}] {}",
            task.id,
            status(task.status),
            task.title
        ));
    }
    if context.tasks.is_empty() {
        lines.push("  (none)".to_string());
    }
    if !context.relations.is_empty() {
        lines.push(String::new());
        lines.push(format!("RELATIONS ({}):", context.relations.len()));
        for edge in &context.relations {
            lines.push(format!(
                "  #{} -{}-> #{}",
                edge.from,
                relation(edge.kind),
                edge.to
            ));
        }
    }
    lines.push(String::new());
    lines.push("CURRENT TASK:".to_string());
    if let Some(task) = &context.focus {
        lines.push(format!(
            "  #{} [{}] {}",
            task.id,
            status(task.status),
            task.title
        ));
        lines.push(format!("  created: {}", format_utc(task.created_at)));
        lines.push(format!("  origin: {}", task.origin));
        lines.push(format!("  request: {}", task.request));
        lines.push(format!("  acceptance: {}", task.acceptance));
        if let Some(report) = &task.report {
            lines.push(format!(
                "  report at: {}",
                task.report_at
                    .map(format_utc)
                    .unwrap_or_else(|| "-".to_string())
            ));
            lines.push(format!("  report: {:?}", report.status));
            lines.push(format!("  summary: {}", report.summary));
            lines.push(format!("  artifacts: {}", report.artifacts.join(", ")));
            lines.push(format!("  evidence: {}", report.evidence.join("; ")));
            lines.push(format!("  questions: {}", report.open_questions.join("; ")));
        }
        if let Some(close) = &task.close {
            lines.push(format!(
                "  closed at: {}",
                task.closed_at
                    .map(format_utc)
                    .unwrap_or_else(|| "-".to_string())
            ));
            lines.push(format!(
                "  closed: {} — {}",
                if close.accepted {
                    "accepted"
                } else {
                    "rejected"
                },
                close.note
            ));
        }
    } else {
        lines.push("  (none)".to_string());
    }
    if !context.recall.is_empty() {
        lines.push(String::new());
        lines.push("RECALLED EVENTS:".to_string());
        for event in &context.recall {
            lines.push(format!(
                "  #{} [{}] {} scope={}: {}",
                event.id,
                label(event.kind),
                format_utc(event.at),
                event.scope,
                event.text
            ));
            if let Some(target) = event.closes {
                lines.push(format!("    closes: #{target}"));
            }
            if let Some(data) = &event.data {
                lines.push(format!("    data: {data}"));
            }
        }
    }
    if !context.artifacts.is_empty() {
        lines.push(String::new());
        lines.push("ARTIFACT READS:".to_string());
        for artifact in &context.artifacts {
            lines.push(format!(
                "  {} ({} bytes{})",
                artifact.path,
                artifact.text.len(),
                if artifact.truncated {
                    ", truncated"
                } else {
                    ""
                }
            ));
            if let Some(error) = &artifact.error {
                lines.push(format!("    error: {error}"));
            } else {
                for line in artifact.text.lines() {
                    lines.push(format!("    {line}"));
                }
            }
        }
    }
    lines.push(String::new());
    lines.push("CURRENT INPUT:".to_string());
    lines.push(format!("  {}", context.input));
    lines.join("\n")
}

pub fn parse_decision(raw: &str) -> Decision {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|text| text.trim())
        .unwrap_or(trimmed);
    let stripped = stripped.strip_suffix("```").unwrap_or(stripped).trim();
    let candidate = match (stripped.find('{'), stripped.rfind('}')) {
        (Some(start), Some(end)) if end > start => &stripped[start..=end],
        _ => stripped,
    };
    match serde_json::from_str::<Decision>(candidate) {
        Ok(decision) => decision,
        Err(_) => Decision {
            reply: trimmed.to_string(),
            actions: Vec::new(),
        },
    }
}

pub struct OpenAiBrain {
    base: String,
    key: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl OpenAiBrain {
    pub fn from_env() -> Option<Self> {
        let (base, key, model) = llm_env()?;
        Some(Self::new(base, key, model))
    }

    pub fn new(base: impl Into<String>, key: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("build llm client");
        Self {
            base: base.into(),
            key: key.into(),
            model: model.into(),
            client,
        }
    }
}

impl Brain for OpenAiBrain {
    fn decide(&self, context: &EjaContext) -> Decision {
        let messages = vec![
            json!({"role": "system", "content": SYSTEM_PROMPT}),
            json!({"role": "user", "content": render_context(context)}),
        ];
        let body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
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
            .and_then(|response| response.json::<Value>());
        match response {
            Ok(value) => {
                let raw = value
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                parse_decision(raw)
            }
            Err(error) => Decision {
                reply: format!("[EJA model error] {error}"),
                actions: Vec::new(),
            },
        }
    }
}
