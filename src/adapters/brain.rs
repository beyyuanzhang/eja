use crate::{format_utc, Brain, Decision, EjaContext};
use serde_json::json;
use std::time::Duration;

use super::{label, llm_env, status, Control, OpenAiModel};

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
an artifact, observing a read-only connector tool or dispatching an AJA returns \
observations for your next decision. Inspect those observations before deciding \
acceptance. A continuation is a new task: when a report says the work is \
unfinished, open the next task and relate it to the old one with \
{\"op\":\"relate\",\"from\":NEW,\"to\":OLD,\"kind\":\"continues\"}. \
To finish, return your final reply with no observation-producing actions. \
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
{\"op\":\"read_artifact\",\"path\":\"out.md\"}; \
{\"op\":\"observe\",\"tool\":\"mcp__server__tool\",\"arguments\":{}}.";

struct Section {
    title: &'static str,
    counted: bool,
    items: Vec<Vec<String>>,
}

fn section(title: &'static str, counted: bool, items: Vec<Vec<String>>) -> Section {
    Section {
        title,
        counted,
        items,
    }
}

fn one_line(text: String) -> Vec<Vec<String>> {
    vec![vec![text]]
}

pub fn render_context(context: &EjaContext) -> String {
    render_context_within(context, usize::MAX)
}

/// Render the projection under a byte budget. A truncated section reports how
/// many items were kept and how many exist, so nothing is silently dropped and
/// the model can fetch the rest with recall or read_task.
pub fn render_context_within(context: &EjaContext, budget: usize) -> String {
    let mut sections = Vec::new();
    sections.push(section(
        "NOW (UTC)",
        false,
        one_line(format!("  {}", format_utc(context.now))),
    ));
    sections.push(section(
        "CURRENT INPUT",
        false,
        one_line(format!("  {}", context.input)),
    ));
    sections.push(section(
        "CURRENT GOAL",
        false,
        one_line(context.goal.as_ref().map_or_else(
            || "  (none)".to_string(),
            |goal| format!("  #{} {}", goal.id, goal.text),
        )),
    ));

    let mut task_lines = Vec::new();
    if let Some(task) = &context.focus {
        task_lines.push(format!(
            "  #{} [{}] {}",
            task.id,
            status(task.status),
            task.title
        ));
        task_lines.push(format!("  created: {}", format_utc(task.created_at)));
        task_lines.push(format!("  origin: {}", task.origin));
        task_lines.push(format!("  request: {}", task.request));
        task_lines.push(format!("  acceptance: {}", task.acceptance));
        if let Some(report) = &task.report {
            task_lines.push(format!(
                "  report at: {}",
                task.report_at
                    .map(format_utc)
                    .unwrap_or_else(|| "-".to_string())
            ));
            task_lines.push(format!("  report: {:?}", report.status));
            task_lines.push(format!("  summary: {}", report.summary));
            task_lines.push(format!("  artifacts: {}", report.artifacts.join(", ")));
            task_lines.push(format!("  evidence: {}", report.evidence.join("; ")));
            task_lines.push(format!("  questions: {}", report.open_questions.join("; ")));
            if !report.tools.is_empty() {
                let names: Vec<&str> = report
                    .tools
                    .iter()
                    .take(8)
                    .map(|call| call.tool.as_str())
                    .collect();
                let more = if report.tools.len() > names.len() {
                    ", …"
                } else {
                    ""
                };
                task_lines.push(format!(
                    "  observed tools: {} calls — {}{}",
                    report.tools.len(),
                    names.join(", "),
                    more
                ));
            }
        }
        if let Some(close) = &task.close {
            task_lines.push(format!(
                "  closed at: {}",
                task.closed_at
                    .map(format_utc)
                    .unwrap_or_else(|| "-".to_string())
            ));
            task_lines.push(format!(
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
        task_lines.push("  (none)".to_string());
    }
    sections.push(section("CURRENT TASK", false, vec![task_lines]));

    sections.push(section(
        "ACTIVE IDENTITY",
        true,
        context
            .identity
            .iter()
            .map(|event| {
                vec![format!(
                    "  #{} [{}] {}: {}",
                    event.id,
                    label(event.kind),
                    format_utc(event.at),
                    event.text
                )]
            })
            .collect(),
    ));
    sections.push(section(
        "OPEN COMMITMENTS",
        true,
        context
            .commitments
            .iter()
            .map(|event| {
                vec![format!(
                    "  #{} {}: {}",
                    event.id,
                    format_utc(event.at),
                    event.text
                )]
            })
            .collect(),
    ));

    let mut recall_items = Vec::new();
    for event in &context.recall {
        let mut item = vec![format!(
            "  #{} [{}] {} scope={}: {}",
            event.id,
            label(event.kind),
            format_utc(event.at),
            event.scope,
            event.text
        )];
        if let Some(target) = event.closes {
            item.push(format!("    closes: #{target}"));
        }
        if let Some(data) = &event.data {
            item.push(format!("    data: {data}"));
        }
        recall_items.push(item);
    }
    sections.push(section("RECALLED EVENTS", true, recall_items));

    let mut observation_items = Vec::new();
    for observation in &context.observations {
        let mut text = format!("    {}", observation.result);
        if text.len() > 2000 {
            text.truncate(2000);
            text.push('…');
        }
        observation_items.push(vec![
            format!("  {} {}", observation.tool, observation.arguments),
            text,
        ]);
    }
    sections.push(section("OBSERVATIONS", true, observation_items));

    sections.push(section(
        "PENDING PROPOSALS",
        true,
        context
            .proposals
            .iter()
            .map(|proposal| {
                vec![format!(
                    "  proposal #{} [{}] target={} turn={}: {}",
                    proposal.id,
                    format!("{:?}", proposal.kind).to_lowercase(),
                    proposal
                        .target_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    proposal.turn,
                    proposal.text
                )]
            })
            .collect(),
    ));
    sections.push(section(
        "TASK INDEX",
        true,
        context
            .tasks
            .iter()
            .map(|task| {
                vec![format!(
                    "  #{} [{}] {}",
                    task.id,
                    status(task.status),
                    task.title
                )]
            })
            .collect(),
    ));
    sections.push(section(
        "RELATIONS",
        true,
        context
            .relations
            .iter()
            .map(|edge| {
                vec![format!(
                    "  #{} -{}-> #{}",
                    edge.from,
                    relation(edge.kind),
                    edge.to
                )]
            })
            .collect(),
    ));

    let mut artifact_items = Vec::new();
    for artifact in &context.artifacts {
        let mut item = vec![format!(
            "  {} ({} bytes{})",
            artifact.path,
            artifact.text.len(),
            if artifact.truncated {
                ", truncated"
            } else {
                ""
            }
        )];
        if let Some(error) = &artifact.error {
            item.push(format!("    error: {error}"));
        } else {
            for line in artifact.text.lines() {
                item.push(format!("    {line}"));
            }
        }
        artifact_items.push(item);
    }
    sections.push(section("ARTIFACT READS", true, artifact_items));

    fit_sections(&sections, budget)
}

fn fit_sections(sections: &[Section], budget: usize) -> String {
    let mut keep: Vec<usize> = sections.iter().map(|section| section.items.len()).collect();
    let mut text = join_sections(sections, &keep);
    while text.len() > budget {
        let Some(index) = (0..keep.len()).rev().find(|&index| keep[index] > 0) else {
            break;
        };
        if text.len() > budget.saturating_mul(2) && keep[index] > 1 {
            keep[index] = 0;
        } else {
            keep[index] -= 1;
        }
        text = join_sections(sections, &keep);
    }
    text
}

fn join_sections(sections: &[Section], keep: &[usize]) -> String {
    let mut out = String::new();
    for (section, &shown) in sections.iter().zip(keep) {
        let total = section.items.len();
        let header = match (section.counted, shown >= total) {
            (false, _) => format!("{}:", section.title),
            (true, true) => format!("{} ({}):", section.title, total),
            (true, false) => format!("{} ({} of {}):", section.title, shown, total),
        };
        out.push_str(&header);
        out.push('\n');
        if total == 0 {
            out.push_str("  (none)\n\n");
            continue;
        }
        for item in section.items.iter().take(shown) {
            for line in item {
                out.push_str(line);
                out.push('\n');
            }
        }
        if shown < total {
            out.push_str(&format!("  … {} more\n", total - shown));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
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
    model: OpenAiModel,
    context_budget: usize,
}

impl OpenAiBrain {
    pub fn from_env() -> Option<Self> {
        let (base, key, model) = llm_env()?;
        Some(Self::new(base, key, model))
    }
    pub fn new(base: impl Into<String>, key: impl Into<String>, model: impl Into<String>) -> Self {
        // The consumer decides how much context it can take; core never picks a size.
        let context_budget = std::env::var("EJA_CONTEXT_BYTES")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(usize::MAX);
        Self {
            model: OpenAiModel::new(base, key, model),
            context_budget,
        }
    }
}

impl Brain for OpenAiBrain {
    fn decide(&self, context: &EjaContext) -> Decision {
        let messages = vec![
            json!({"role":"system","content":SYSTEM_PROMPT}),
            json!({"role":"user","content":render_context_within(context, self.context_budget)}),
        ];
        let control = Control::new(
            Duration::from_secs(180),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        match self
            .model
            .request(&messages, &[], crate::aja::FRAME_LIMIT, &control)
        {
            Ok(value) => parse_decision(
                value
                    .pointer("/choices/0/message/content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            ),
            Err(error) => Decision {
                reply: format!("[EJA model error] {error}"),
                actions: vec![],
            },
        }
    }
}
