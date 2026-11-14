use crate::{format_utc, Action, Brain, Decision, EjaContext, LookSource, Selector};
use serde_json::json;
use std::time::Duration;

use super::{label, llm_env, Control, Model, OpenAiModel};

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
leave a record of what they did. The record is yours to read: a dispatch returns the \
host's digest of it, and you may look deeper at any node. Treat the node's last \
message as its account, not a verified outcome; the record and the decision are yours. You observe, you never execute: you may read the log, node records, and the project itself (read_file, list_dir), and every effect happens inside an AJA node. There is no fallback executor. A correction targets an explicit event id; never invent one. Identity changes \
(goal, commitment, correction) are proposals: propose them, then confirm \
only when its id is in HOST APPROVED PROPOSALS. Never confirm a proposal in the same turn it was \
created. \
The current input is separate from the persistent goal. Use a goal/commitment \
proposal without target_id to add one; with target_id to replace an active event \
of the same kind. Use correction with target_id only to withdraw that event. \
The proposal text is the replacement content, or the withdrawal reason. \
Actions execute in their listed order. Creating a node, group or proposal returns its allocated id in the next projection and continues this turn; use that id rather than guessing it. Every read is one action: a look names \
a source (log, node or world), a target and a selector. A node look reads that \
node's own files or the workspace it left behind. Dispatching an AJA also \
returns an observation, and several dispatches in one decision run together. A continuation is represented as a new task related to \
the previous one with \
{\"op\":\"relate\",\"from\":NEW,\"to\":OLD,\"kind\":\"continues\"}. \
Tasks are execution nodes; open_group creates a non-executable container. part_of targets a group. A group can be closed after its children settle; judge its own acceptance contract. Execution edges are fixed before dispatch; continues has exactly one finished predecessor, and depends_on requires an accepted predecessor before dispatch. Open a continuation with run:false, relate it, then dispatch it. World reads are governed only by host capabilities. Read older conversation with look log history or all, using range to page event indices. To finish, return your final reply with no observation-producing actions. \
Return ONLY a JSON object: {\"reply\":\"...\",\"actions\":[...]}. Action shapes: \
{\"op\":\"propose\",\"kind\":\"goal|commitment|correction\",\"target_id\":1,\"text\":\"...\"}; \
{\"op\":\"confirm\",\"proposal_id\":1}; \
{\"op\":\"reject\",\"proposal_id\":1,\"note\":\"...\"}; \
{\"op\":\"decision|relationship\",\"text\":\"...\"}; \
{\"op\":\"open_group\",\"title\":\"...\",\"acceptance\":\"...\"}; \
{\"op\":\"open_task\",\"title\":\"...\",\"request\":\"...\",\"acceptance\":\"...\",\"run\":true}; \
{\"op\":\"dispatch\",\"task_id\":1}; \
{\"op\":\"close_task\",\"task_id\":1,\"accepted\":true,\"note\":\"...\"}; \
{\"op\":\"focus\",\"task_id\":1}; \
{\"op\":\"look\",\"source\":\"log\",\"target\":\"task:1\"}; \
{\"op\":\"look\",\"source\":\"node\",\"target\":\"1/report/out.md\"}; \
{\"op\":\"look\",\"source\":\"node\",\"target\":\"3/context.jsonl\",\"selector\":{\"select\":\"match\",\"text\":\"PO-1010\",\"before\":2,\"after\":4}}; \
{\"op\":\"look\",\"source\":\"node\",\"target\":\"1/report/out.md\",\"selector\":{\"select\":\"range\",\"from\":0,\"lines\":40}}; \
{\"op\":\"look\",\"source\":\"world\",\"target\":\"list_dir\",\"arguments\":{\"path\":\".\"}}; \
{\"op\":\"look\",\"source\":\"world\",\"target\":\"read_file\",\"arguments\":{\"path\":\"README.md\"}}; \
{\"op\":\"look\",\"source\":\"world\",\"target\":\"mcp__server__tool\",\"arguments\":{}}. \
Preconditions: dispatch runs only a task in open state; a task that has a node \
record is waiting for your judgment, so close it or open a new task for more \
work. close_task requires a waiting node, an open node being cancelled, or a group whose children are terminal. confirm needs a host-approved pending proposal from an \
earlier turn. Log pages expose from/next/total; continue at next. Oversized events are listed in partial_events: read event:<id> with selector {\"select\":\"bytes\",\"from\":0,\"bytes\":4096}, then continue at next_offset. Byte views contain serialized event text.";

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

/// Render a bounded projection with total section counts and an explicit cut
/// notice. Omitted records remain discoverable through typed log lookups.
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
    if !context.approved_proposals.is_empty() {
        sections.push(section(
            "HOST APPROVED PROPOSALS",
            false,
            one_line(format!("  {:?}", context.approved_proposals)),
        ));
    }
    sections.push(section(
        "CURRENT GOAL",
        false,
        one_line(context.goal.as_ref().map_or_else(
            || "  (none)".to_string(),
            |goal| format!("  #{} {}", goal.id, goal.text),
        )),
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
    let mut task_lines = Vec::new();
    if let Some(task) = &context.focus {
        task_lines.push(format!(
            "  #{} [{:?}/{}] {}",
            task.id,
            task.kind,
            task.status.as_str(),
            task.title
        ));
        task_lines.push(format!("  created: {}", format_utc(task.created_at)));
        task_lines.push(format!("  request: {}", task.request));
        task_lines.push(format!("  acceptance: {}", task.acceptance));
        if let Some(digest) = &task.digest {
            task_lines.push(format!(
                "  node record at: {}",
                task.digest_at
                    .map(format_utc)
                    .unwrap_or_else(|| "-".to_string())
            ));
            for line in digest.lines() {
                task_lines.push(format!("  record: {line}"));
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

    // What the host could not do, in EJA's own words from the last decision: the
    // state refused it, or it could not be read as an action. EJA sees this before
    // deciding again, so an impossible action is not a dead end.
    if !context.refused.is_empty() {
        sections.push(section(
            "REFUSED",
            false,
            context
                .refused
                .iter()
                .map(|note| vec![format!("  {note}")])
                .collect(),
        ));
    }

    // Evidence sits before the index: when a budget forces a trim, what EJA
    // actually looked at survives and old index entries are dropped first.
    let mut evidence_items = Vec::new();
    for observation in &context.evidence {
        let selector = match &observation.selector {
            Selector::Whole => String::new(),
            Selector::Range { from, lines } => format!(" range {from}+{lines}"),
            Selector::Bytes { from, bytes } => format!(" bytes {from}+{bytes}"),
            Selector::Match { text, .. } => format!(" match \"{text}\""),
        };
        let mut item = vec![format!(
            "  [{} {}]{}",
            source_label(observation.source),
            observation.target,
            selector
        )];
        if let Some(error) = &observation.error {
            item.push(format!("    error: {error}"));
        } else if observation.source == LookSource::Log
            && observation.result.get("events").is_some()
        {
            item.push(format!(
                "    page from={} next={} total={} truncated={}",
                observation.result["from"],
                observation.result["next"],
                observation.result["total"],
                observation.result["truncated"]
            ));
            if let Some(partial) = observation.result.get("partial_events") {
                item.push(format!(
                    "    oversized events (read event:<id> with bytes): {partial}"
                ));
            }
            let events: Vec<crate::Event> = observation
                .result
                .get("events")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
            for event in events {
                item.push(format!(
                    "    #{} [{}] {} scope={}: {}",
                    event.id,
                    label(event.kind),
                    format_utc(event.at),
                    event.scope,
                    event.text
                ));
                if let Some(target) = event.closes {
                    item.push(format!("      closes: #{target}"));
                }
                if let Some(data) = &event.data {
                    item.push(format!("      data: {data}"));
                }
            }
        } else {
            item.push(format!("    {}", observation.result));
        }
        evidence_items.push(item);
    }
    sections.push(section("EVIDENCE", true, evidence_items));

    sections.push(section(
        "ACTIVE IDENTITY",
        true,
        context
            .identity
            .iter()
            .rev()
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
        "RECENT CONVERSATION",
        true,
        context
            .history
            .iter()
            .map(|event| {
                vec![format!(
                    "  #{} [{}] {}",
                    event.id,
                    label(event.kind),
                    event.text
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
                    "  #{} [{:?}/{}] {}",
                    task.id,
                    task.kind,
                    task.status.as_str(),
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

    fit_sections(&sections, budget)
}

/// Assemble the view in priority order. The declared budget is the only limit:
/// a record that does not fit is cut where the budget ends and the cut is
/// stated. Section counts describe the uncut projection.
fn fit_sections(sections: &[Section], budget: usize) -> String {
    let footer = "\n… view budget reached; use look log history/all or a record address for omitted content\n";
    let mut full = String::new();
    for section in sections {
        let header = if section.counted {
            format!("{} ({}):\n", section.title, section.items.len())
        } else {
            format!("{}:\n", section.title)
        };
        full.push_str(&header);
        if section.items.is_empty() {
            full.push_str("  (none)\n");
        }
        for item in &section.items {
            for line in item {
                full.push_str(line);
                full.push('\n');
            }
        }
        full.push('\n');
    }
    if full.len() <= budget {
        return full.trim_end().to_string();
    }
    // The rendered-text cap includes headers, notices and UTF-8 bytes.
    let kept = budget.saturating_sub(footer.len());
    let mut out = super::cut_chars(&full, kept).to_string();
    out.push_str(super::cut_chars(footer, budget.saturating_sub(out.len())));
    out
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
    let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) else {
        // Not a decision object: a plain reply is a legitimate answer.
        return Decision {
            reply: trimmed.to_string(),
            actions: Vec::new(),
            refused: Vec::new(),
        };
    };
    if !value.is_object() {
        return Decision {
            reply: trimmed.to_string(),
            actions: Vec::new(),
            refused: Vec::new(),
        };
    }
    let reply = value
        .get("reply")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut actions = Vec::new();
    let mut notes = Vec::new();
    match value.get("actions").map(|actions| actions.as_array()) {
        Some(Some(list)) => {
            // One unreadable action does not discard the rest of the decision:
            // the readable actions still run and the loss is recorded in the reply.
            for item in list {
                match serde_json::from_value::<Action>(item.clone()) {
                    Ok(action) => actions.push(action),
                    Err(error) => notes.push(format!("{error}: {item}")),
                }
            }
        }
        Some(None) => notes.push("actions was not a list".to_string()),
        None => {}
    }
    Decision {
        reply,
        actions,
        refused: notes,
    }
}

/// Conservative adapter-level guard: large enough for normal sessions, small
/// enough that a pathological log does not hit a provider limit. Override with
/// `EJA_CONTEXT_BYTES`.
const DEFAULT_CONTEXT_BYTES: usize = 200_000;

/// One decision may not outlive this; it is a host resource, not a result cap.
const DECIDE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct OpenAiBrain {
    model: OpenAiModel,
    context_budget: usize,
}

impl OpenAiBrain {
    pub fn from_env() -> crate::Result<Option<Self>> {
        let Some((base, key, model)) = llm_env() else {
            return Ok(None);
        };
        let mut brain = Self::new(base, key, model);
        if let Ok(raw) = std::env::var("EJA_CONTEXT_BYTES") {
            let budget = raw.parse().map_err(|_| "invalid EJA_CONTEXT_BYTES")?;
            brain = brain.with_context_budget(budget)?;
        }
        Ok(Some(brain))
    }
    pub fn new(base: impl Into<String>, key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            model: OpenAiModel::new(base, key, model),
            context_budget: DEFAULT_CONTEXT_BYTES,
        }
    }
    pub fn with_context_budget(mut self, budget: usize) -> crate::Result<Self> {
        if budget == 0 {
            return Err("EJA_CONTEXT_BYTES must be positive".into());
        }
        self.context_budget = budget;
        Ok(self)
    }
}

impl Brain for OpenAiBrain {
    fn decide(&self, context: &EjaContext) -> Decision {
        let control = Control::new(
            DECIDE_TIMEOUT,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        self.decide_controlled(context, &control)
    }
    fn decide_controlled(&self, context: &EjaContext, parent: &Control) -> Decision {
        let messages = vec![
            json!({"role":"system","content":SYSTEM_PROMPT}),
            json!({"role":"user","content":render_context_within(context, self.context_budget)}),
        ];
        let control = parent.child(
            DECIDE_TIMEOUT,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        match self.model.complete(
            &messages,
            &[],
            &crate::aja::Limits {
                response_bytes: crate::aja::FRAME_LIMIT,
                ..Default::default()
            },
            &control,
            true,
        ) {
            Ok(value) => parse_decision(
                value
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            ),
            Err(error) => Decision {
                reply: format!("[EJA model error] {error}"),
                actions: vec![],
                refused: Vec::new(),
            },
        }
    }
}

fn source_label(source: LookSource) -> &'static str {
    match source {
        LookSource::Log => "log",
        LookSource::Node => "node",
        LookSource::World => "world",
    }
}
