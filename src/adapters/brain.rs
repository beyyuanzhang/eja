//! EJA's model transport: one bounded projection in, one typed decision out.
use crate::fold;
use crate::{
    format_utc, Action, Brain, Decision, EjaContext, Observation, ObservationKind, Selector,
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

use super::{label, llm_env, llm_headers, Control, Model, OpenAiModel};

const fn relation(kind: crate::RelationKind) -> &'static str {
    match kind {
        crate::RelationKind::PartOf => "part_of",
        crate::RelationKind::DependsOn => "depends_on",
        crate::RelationKind::Continues => "continues",
        crate::RelationKind::RelatesTo => "relates_to",
        crate::RelationKind::Supersedes => "supersedes",
        crate::RelationKind::Contradicts => "contradicts",
    }
}

const fn kind_label(kind: ObservationKind) -> &'static str {
    match kind {
        ObservationKind::Look => "look",
        ObservationKind::Witness => "witness",
        ObservationKind::Reconcile => "reconcile",
    }
}

fn event_address(id: u64) -> String {
    format!("log:event:{id}")
}

fn task_address(id: u64) -> String {
    format!("log:task:{id}")
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
address -- log:event:12, log:task:1, log:history, record:3/report.md (one frozen \
file of aja 3), world:src/out.txt (one path of the project) -- or a host read tool \
such as read_file, list_dir or a granted connector, which selects its result through \
its own arguments. A world address takes no selector; a log page takes whole \
(newest 40) or range, and event:<id> also takes bytes; a frozen record file takes \
whole, range or match. \
Identity changes (goal, commitment, correction) are proposals: propose them, then \
confirm only when the proposal is marked approved in PROPOSALS, and never \
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
An aja opened with evidence asks the host for the observation its judgment needs; \
without the host's observation an acceptance is undecidable, not failed, and a \
rejection needs no evidence. A run that left an isolated world can be reconciled \
explicitly; the outcome, conflicts included, is recorded evidence. \
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
{\"op\":\"look\",\"target\":\"world:src/main.rs\"}; \
{\"op\":\"look\",\"target\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}; \
{\"op\":\"look\",\"target\":\"mcp__server__tool\",\"arguments\":{}}. \
Preconditions: dispatch runs only a task in open state; a task that has a aja \
record is waiting for judgment, so close it or open a new task for more work. \
close_task requires a waiting aja, an open aja being cancelled, a group whose \
children are terminal, and the evidence a task asked for. confirm needs a \
proposal from an earlier turn that the host approved for this turn. Log pages \
expose from/next/total; continue at next. Oversized events are listed in \
partial_events: read event:<id> with selector {\"select\":\"bytes\",\"from\":0,\
\"bytes\":4096}, then continue at next_offset. Byte views contain serialized event text.";

/// The projection's regions. The declaration order is the priority the fold
/// protects: what this turn is about and what EJA just looked at come before any
/// index, so the self survives budget pressure and the indexes fold first. One
/// place states both the order and the title, so they cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Now,
    Input,
    Goal,
    Identity,
    Task,
    Evidence,
    Refused,
    Commitments,
    Proposals,
    Conversation,
    Index,
    Relations,
}

impl Slot {
    const fn title(self) -> &'static str {
        match self {
            Self::Now => "NOW",
            Self::Input => "INPUT",
            Self::Goal => "GOAL",
            Self::Identity => "SAID",
            Self::Task => "TASK",
            Self::Evidence => "EVIDENCE",
            Self::Refused => "REFUSED",
            Self::Commitments => "COMMITMENTS",
            Self::Proposals => "PROPOSALS",
            Self::Conversation => "CONVERSATION",
            Self::Index => "TASKS",
            Self::Relations => "RELATIONS",
        }
    }
}

/// One line of a section. A line whose fact has an address says where it lives, so
/// a section that the budget folds can still be unfolded.
struct Item<'a> {
    /// The address a folded section names: sometimes a pattern built for this
    /// render (an event or task id), sometimes the observation's own target.
    address: Option<std::borrow::Cow<'a, str>>,
    lines: Vec<String>,
}

struct Section<'a> {
    slot: Slot,
    counted: bool,
    items: Vec<Item<'a>>,
}

const fn plain(lines: Vec<String>) -> Item<'static> {
    Item {
        address: None,
        lines,
    }
}

fn addressed<'a>(address: impl Into<std::borrow::Cow<'a, str>>, lines: Vec<String>) -> Item<'a> {
    Item {
        address: Some(address.into()),
        lines,
    }
}

const fn section(slot: Slot, counted: bool, items: Vec<Item<'_>>) -> Section<'_> {
    Section {
        slot,
        counted,
        items,
    }
}

/// One section rendered at record scale, written straight into its sink so the
/// same code both counts it and renders it.
fn section_into(out: &mut impl std::fmt::Write, section: &Section<'_>) -> std::fmt::Result {
    if section.counted {
        writeln!(out, "{} ({}):", section.slot.title(), section.items.len())?;
    } else {
        writeln!(out, "{}:", section.slot.title())?;
    }
    if section.items.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for item in &section.items {
        for line in &item.lines {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// One section rendered at record scale.
fn section_text(section: &Section<'_>) -> String {
    let mut out = String::new();
    let _ = section_into(&mut out, section);
    out
}

/// The bytes one section would render to, without rendering it.
fn section_len(section: &Section<'_>) -> usize {
    let mut count = crate::measure::Count::default();
    let _ = section_into(&mut count, section);
    count.0
}

/// The addresses a section's items live at, as one range.
fn span(section: &Section<'_>) -> Option<String> {
    let mut first = None;
    let mut last = None;
    for address in section
        .items
        .iter()
        .filter_map(|item| item.address.as_deref())
    {
        first.get_or_insert(address);
        last = Some(address);
    }
    let first = first?;
    let last = last.unwrap_or(first);
    Some(if first == last {
        first.to_string()
    } else {
        format!("{first} .. {last}")
    })
}

/// One section at index scale: its title, its size and the address that unfolds
/// it. Unfolding is one address lookup, so the footer states the rule once.
#[must_use]
fn section_line(section: &Section<'_>) -> String {
    let count = section.items.len();
    let noun = if count == 1 { "item" } else { "items" };
    span(section).map_or_else(
        || format!("  {} ({count} {noun})", section.slot.title()),
        |span| format!("  {} ({count} {noun}, {span})", section.slot.title()),
    )
}

#[must_use]
pub fn render_context(context: &EjaContext) -> String {
    render_context_within(context, usize::MAX)
}

/// Render a bounded projection with total section counts and an explicit landing
/// scale. Omitted regions stay addressable and discoverable through their ids.
#[must_use]
pub fn render_context_within(context: &EjaContext, budget: usize) -> String {
    let mut sections = Vec::new();
    sections.push(section(
        Slot::Now,
        false,
        vec![plain(vec![format!("  {}", format_utc(context.now))])],
    ));
    sections.push(section(
        Slot::Input,
        false,
        vec![plain(vec![format!("  {}", context.input)])],
    ));
    // A fact the subject does not have is not rendered: a projection of nothing
    // says nothing, so nothing here describes a shape rather than a content.
    sections.push(section(
        Slot::Goal,
        false,
        context.goal.map_or_else(Vec::new, |goal| {
            vec![addressed(
                event_address(goal.id),
                vec![format!("  #{} {}", goal.id, goal.text)],
            )]
        }),
    ));

    sections.push(section(
        Slot::Commitments,
        true,
        context
            .commitments
            .iter()
            .map(|event| {
                addressed(
                    event_address(event.id),
                    vec![format!(
                        "  #{} {}: {}",
                        event.id,
                        format_utc(event.at),
                        event.text
                    )],
                )
            })
            .collect(),
    ));
    sections.push(section(
        Slot::Proposals,
        true,
        context
            .proposals
            .iter()
            .map(|proposal| {
                addressed(
                    event_address(proposal.id),
                    vec![format!(
                        "  proposal #{} [{}] target={} turn={}{}: {}",
                        proposal.id,
                        format!("{:?}", proposal.kind).to_lowercase(),
                        proposal
                            .target_id
                            .map_or_else(|| "-".to_string(), |id| id.to_string()),
                        proposal.turn,
                        if proposal.approved {
                            " approved"
                        } else {
                            " needs host approval"
                        },
                        proposal.text
                    )],
                )
            })
            .collect(),
    ));
    // The focus is one of the tasks, not a second copy of it.
    let focused = context.focus.and_then(|id| context.task(id));
    let task_items = focused.map_or_else(Vec::new, |task| {
        let mut lines = vec![
            format!(
                "  #{} [{:?}/{}] {}",
                task.id,
                task.kind,
                task.status.as_str(),
                task.title
            ),
            format!("  created: {}", format_utc(task.created_at)),
            format!("  request: {}", task.request),
            format!("  acceptance: {}", task.acceptance),
        ];
        if let Some(run) = &task.run {
            if let Some(digest) = &run.digest {
                lines.push(format!(
                    "  aja record at: {}",
                    run.digest_at.map_or_else(|| "-".to_string(), format_utc)
                ));
                for line in digest.lines() {
                    lines.push(format!("  record: {line}"));
                }
            }
            if let Some(evidence) = &run.evidence {
                lines.push(format!("  evidence: {evidence}"));
                match (&run.evidence_result, run.evidence_id) {
                    (Some(result), Some(id)) => {
                        lines.push(format!("  evidence observed #{id}: {result}"));
                    }
                    _ => lines.push(
                        "  evidence observed: no — an acceptance is undecidable until the host answers, a rejection needs none"
                            .to_string(),
                    ),
                }
            }
        }
        if let Some(close) = &task.close {
            lines.push(format!(
                "  closed at: {}",
                task.closed_at.map_or_else(|| "-".to_string(), format_utc)
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
            vec![addressed(task_address(task.id), lines)]
    });
    sections.push(section(Slot::Task, false, task_items));

    // What the host could not do, in EJA's own words from the last decision: the
    // state refused it, or it could not be read as an action. EJA sees this before
    // deciding again, so an impossible action is not a dead end.
    if !context.refused.is_empty() {
        sections.push(section(
            Slot::Refused,
            false,
            context
                .refused
                .iter()
                .map(|note| plain(vec![format!("  {note}")]))
                .collect(),
        ));
    }

    // Evidence sits before the index: when a budget forces a trim, what EJA
    // actually looked at survives and old index entries are dropped first.
    let mut evidence: Vec<Item<'_>> = context
        .evidence
        .iter()
        .map(|observation| {
            // An observation that names an address is unfolded by that address;
            // one that names a host tool has none to give.
            let lines = evidence_lines(observation);
            if crate::Address::parse(observation.target).is_none() {
                return plain(lines);
            }
            Item {
                address: Some(std::borrow::Cow::Borrowed(observation.target)),
                lines,
            }
        })
        .collect();
    // An observation this version cannot read is stated, never dropped.
    if context.unreadable_evidence > 0 {
        evidence.push(plain(vec![format!(
            "  {} committed observation(s) this version could not read",
            context.unreadable_evidence
        )]));
    }
    sections.push(section(Slot::Evidence, true, evidence));

    sections.push(section(
        Slot::Identity,
        true,
        context
            .identity
            .iter()
            .rev()
            .map(|event| {
                addressed(
                    event_address(event.id),
                    vec![format!(
                        "  #{} [{}] {}: {}",
                        event.id,
                        label(event.kind),
                        format_utc(event.at),
                        event.text
                    )],
                )
            })
            .collect(),
    ));
    sections.push(section(
        Slot::Conversation,
        true,
        context
            .history
            .iter()
            .map(|event| {
                addressed(
                    event_address(event.id),
                    vec![format!(
                        "  #{} [{}] {}",
                        event.id,
                        label(event.kind),
                        event.text
                    )],
                )
            })
            .collect(),
    ));
    // The index leads with the tasks that still need judgment, then the rest.
    let mut index: Vec<&crate::Task> = context.tasks.iter().collect();
    index.sort_by(|a, b| {
        b.status
            .is_active()
            .cmp(&a.status.is_active())
            .then_with(|| b.id.cmp(&a.id))
    });
    sections.push(section(
        Slot::Index,
        true,
        index
            .iter()
            .map(|task| {
                addressed(
                    task_address(task.id),
                    vec![format!(
                        "  #{} [{:?}/{}] {}",
                        task.id,
                        task.kind,
                        task.status.as_str(),
                        task.title
                    )],
                )
            })
            .collect(),
    ));
    sections.push(section(
        Slot::Relations,
        true,
        context
            .relations
            .iter()
            .map(|edge| {
                addressed(
                    task_address(edge.from),
                    vec![format!(
                        "  #{} -{}-> #{}",
                        edge.from,
                        relation(edge.kind),
                        edge.to
                    )],
                )
            })
            .collect(),
    ));

    // A section the subject has nothing in is not part of the view: the shape of
    // an empty index is not a fact about anyone, and rendering it invites the
    // model to describe its own machinery instead of answering.
    sections.retain(|section| !section.items.is_empty());
    sections.sort_by_key(|section| section.slot);
    folded(&sections, budget)
}

/// One observation as the model reads it: what it is, and what it read.
fn evidence_lines(observation: &Observation) -> Vec<String> {
    let selector = match &observation.selector {
        Selector::Whole => String::new(),
        Selector::Range { from, lines } => format!(" range {from}+{lines}"),
        Selector::Bytes { from, bytes } => format!(" bytes {from}+{bytes}"),
        Selector::Match { text, .. } => format!(" match \"{text}\""),
    };
    let mut lines = vec![format!(
        "  [{} {}]{}",
        kind_label(observation.kind),
        observation.target,
        selector
    )];
    if let Some(error) = &observation.error {
        lines.push(format!("    error: {error}"));
        return lines;
    }
    if observation.target.starts_with("log:") && observation.result.get("events").is_some() {
        lines.push(format!(
            "    page from={} next={} total={} truncated={}",
            observation.result["from"],
            observation.result["next"],
            observation.result["total"],
            observation.result["truncated"]
        ));
        if let Some(partial) = observation.result.get("partial_events") {
            lines.push(format!(
                "    oversized events (read event:<id> with bytes): {partial}"
            ));
        }
        let events: Vec<crate::Event> = observation
            .result
            .get("events")
            .and_then(|value| <Vec<crate::Event>>::deserialize(value).ok())
            .unwrap_or_default();
        for event in events {
            lines.push(format!(
                "    #{} [{}] {} scope={}: {}",
                event.id,
                label(event.kind),
                format_utc(event.at),
                event.scope,
                event.text
            ));
            if let Some(target) = event.closes {
                lines.push(format!("      closes: #{target}"));
            }
            if let Some(data) = &event.data {
                lines.push(format!("      data: {data}"));
            }
        }
    } else {
        lines.push(format!("    {}", observation.result));
    }
    lines
}

/// The projection is one space: sections in priority order, each whole while the
/// declared budget allows, listed when it does not, and never cut mid-line.
fn folded(sections: &[Section<'_>], budget: usize) -> String {
    // Measure without rendering: a section the budget folds into one line is
    // never built in full.
    let regions: Vec<fold::Region> = sections
        .iter()
        .map(|section| {
            fold::Region::new(
                span(section).unwrap_or_default(),
                section_line(section),
                section_len(section),
            )
        })
        .collect();
    let view = fold::assemble(&regions, budget);
    fold::text_within(&view, &regions, "", budget, &|index| {
        section_text(&sections[index])
    })
}

pub fn parse_decision(raw: &str) -> Decision {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("\u{60}\u{60}\u{60}json")
        .or_else(|| trimmed.strip_prefix("\u{60}\u{60}\u{60}"))
        .map_or(trimmed, str::trim);
    let stripped = stripped
        .strip_suffix("\u{60}\u{60}\u{60}")
        .unwrap_or(stripped)
        .trim();
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
                match Action::deserialize(item) {
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
        let mut brain = Self::new(base, key, model).with_headers(llm_headers("LNA_HEADERS"));
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
    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.model = self.model.with_headers(headers);
        self
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
