use crate::{format_utc, Action, Brain, Decision, EjaContext, Selector};
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
Return ONE JSON object: {\"reply\":\"...\",\"actions\":[...]}. \
A decision cannot execute anything: the host validates every action, records it, and \
runs the executions. Actions run in their listed order. Creating a aja, group or \
proposal returns its allocated id in the next projection and continues this turn; \
use that id instead of guessing it. \
A dispatch returns the host's account of one run; the run's record is its evidence. \
Treat a run's last message as its account, not as a verified outcome. \
Every read is one action: a look names a target and a selector. A target is an \
address -- log:event:12, log:task:1, log:history, record:3/4 (one tool result), \
record:3/report.md (one frozen file), world:src/out.txt -- or a host read tool such \
as read_file, list_dir or a granted connector, which selects through arguments. \
Identity changes (goal, commitment, correction) are proposals: propose them, then \
confirm only when the proposal is marked approved in PENDING PROPOSALS, and never \
in the turn that created it. An approval is recorded by the host for one turn; \
model text never is. An approval authorizes one turn, so a proposal that needs one \
ends the turn and the next turn carries it. \
The current input is separate from the persistent goal. Use a goal/commitment \
proposal without target_id to add one; with target_id to replace an active event \
of the same kind. Use correction with target_id only to withdraw that event. \
The proposal text is the replacement content, or the withdrawal reason. \
Execution edges are fixed before dispatch: dispatch runs only an open aja, \
depends_on needs its target done, continues has exactly one predecessor with a \
frozen record, and part_of targets an open group. A group can be closed after its \
children settle; judge its own acceptance contract, not its children's verdicts. \
An aja opened with evidence asks the host for acceptance evidence; without the \
host's observation the judgment is undecidable, not failed. A run that left an \
isolated world can be reconciled explicitly; the outcome, conflicts included, is \
recorded evidence. \
Tasks are execution ajas; open_group creates a non-executable container. \
Dispatches in one decision run in action order; the host may run them at once when \
it has isolated worlds. A continuation is a new task related to the previous one \
with {\"op\":\"relate\",\"from\":NEW,\"to\":OLD,\"kind\":\"continues\"}. \
Action shapes: \
{\"op\":\"propose\",\"kind\":\"goal|commitment|correction\",\"target_id\":1,\"text\":\"...\"}; \
{\"op\":\"confirm\",\"proposal_id\":1}; \
{\"op\":\"reject\",\"proposal_id\":1,\"note\":\"...\"}; \
{\"op\":\"decision|relationship\",\"text\":\"...\"}; \
{\"op\":\"open_group\",\"title\":\"...\",\"acceptance\":\"...\"}; \
{\"op\":\"open_task\",\"title\":\"...\",\"request\":\"...\",\"acceptance\":\"...\",\
\"evidence\":{\"tool\":\"<host tool>\",\"arguments\":{}},\"run\":true}; \
{\"op\":\"reconcile\",\"task_id\":1}; \
{\"op\":\"dispatch\",\"task_id\":1}; \
{\"op\":\"close_task\",\"task_id\":1,\"accepted\":true,\"note\":\"...\"}; \
{\"op\":\"focus\",\"task_id\":1}; \
{\"op\":\"look\",\"target\":\"log:task:1\"}; \
{\"op\":\"look\",\"target\":\"record:3/4\",\"selector\":{\"select\":\"bytes\",\"from\":0,\"bytes\":4096}}; \
{\"op\":\"look\",\"target\":\"world:read_file\",\"arguments\":{\"path\":\"README.md\"}}; \
{\"op\":\"look\",\"target\":\"mcp__server__tool\",\"arguments\":{}}. \
Preconditions: dispatch runs only a task in open state; a task that has a aja \
record is waiting for judgment, so close it or open a new task for more work. \
close_task requires a waiting aja, an open aja being cancelled, a group whose \
children are terminal, and the witness evidence a task asked for. confirm needs a \
proposal from an earlier turn that the host approved for this turn. Log pages \
expose from/next/total; continue at next. Oversized events are listed in \
partial_events: read event:<id> with selector {\"select\":\"bytes\",\"from\":0,\
\"bytes\":4096}, then continue at next_offset. Byte views contain serialized event text.";

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

/// One section rendered at record scale.
fn section_text(section: &Section) -> String {
    let mut out = if section.counted {
        format!("{} ({}):\n", section.title, section.items.len())
    } else {
        format!("{}:\n", section.title)
    };
    if section.items.is_empty() {
        out.push_str("  (none)\n");
    }
    for item in &section.items {
        for line in item {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// One section at index scale: its title and its size. Unfolding is one address
/// lookup, which the footer states once instead of repeating on every line.
fn section_line(section: &Section) -> String {
    format!("{} ({} item)", section.title, section.items.len())
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
                    "  proposal #{} [{}] target={} turn={}{}: {}",
                    proposal.id,
                    format!("{:?}", proposal.kind).to_lowercase(),
                    proposal
                        .target_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    proposal.turn,
                    if proposal.approved {
                        " approved"
                    } else {
                        " needs host approval"
                    },
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
                "  aja record at: {}",
                task.digest_at
                    .map(format_utc)
                    .unwrap_or_else(|| "-".to_string())
            ));
            for line in digest.lines() {
                task_lines.push(format!("  record: {line}"));
            }
        }
        if let Some(witness) = &task.witness {
            task_lines.push(format!("  witness: {witness}"));
            match (&task.witness_result, task.witness_id) {
                (Some(result), Some(id)) => {
                    task_lines.push(format!("  witness evidence #{id}: {result}"))
                }
                _ => task_lines.push(
                    "  witness evidence: not observed — a judgment now would be undecidable"
                        .to_string(),
                ),
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
            address_label(&observation.target),
            observation.target,
            selector
        )];
        if let Some(error) = &observation.error {
            item.push(format!("    error: {error}"));
        } else if observation.target.starts_with("log:")
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

    // The fold keeps the head whole, so the order is the priority: what this
    // turn is about and what EJA just looked at come before any index.
    // The self is not an index: goal and identity survive budget pressure, and
    // the indexes fold first. Nothing here is prose about the subject.
    const ORDER: [&str; 12] = [
        "NOW (UTC)",
        "CURRENT INPUT",
        "CURRENT GOAL",
        "ACTIVE IDENTITY",
        "CURRENT TASK",
        "EVIDENCE",
        "REFUSED",
        "OPEN COMMITMENTS",
        "PENDING PROPOSALS",
        "RECENT CONVERSATION",
        "TASK INDEX",
        "RELATIONS",
    ];
    sections.sort_by_key(|section| {
        ORDER
            .iter()
            .position(|title| *title == section.title)
            .unwrap_or(ORDER.len())
    });
    folded(&sections, budget)
}

/// The projection is one space: sections in priority order, each whole while the
/// declared budget allows, listed when it does not, and never cut mid-line.
fn folded(sections: &[Section], budget: usize) -> String {
    let bodies: Vec<String> = sections.iter().map(section_text).collect();
    let regions: Vec<crate::fold::Region> = sections
        .iter()
        .zip(&bodies)
        .map(|(section, body)| {
            crate::fold::Region::new(section.title, section_line(section), body.len())
        })
        .collect();
    let view = crate::fold::assemble(&regions, budget);
    crate::fold::text_within(&view, &bodies, "", budget)
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
    fn decide(&self, context: &EjaContext, parent: &Control) -> Decision {
        let messages = vec![
            json!({"role":"system","content":SYSTEM_PROMPT}),
            json!({"role":"user","content":render_context_within(context, self.context_budget)}),
        ];
        let control = parent.child(DECIDE_TIMEOUT);
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

/// A target's label is derived from what it is: an address scheme, or the host
/// tool it names. There is no second taxonomy of reads.
fn address_label(target: &str) -> &str {
    target
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .unwrap_or("tool")
}
