//! One renderer for every model-facing view.
//!
//! A view is a space of regions ordered by priority: the newest step first, the
//! current input first, the evidence before the index. The renderer keeps the
//! head whole while the declared budget allows, lists the rest as one line each,
//! and folds the oldest into one address line. A folded region keeps its handle
//! in the log or in the node record, so reading it again is an address lookup,
//! never a re-derivation: the space is folded, not summarized.
//!
//! EJA's projection and an AJA transcript are the same call with a different
//! payload. Nothing here writes state or calls a model.

use crate::aja::Limits;
use crate::{Assignment, Result};
use serde_json::{json, Value};

/// The scale a region is shown at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The region's whole body.
    Record,
    /// One line with the region's handle.
    Index,
    /// One line for the folded tail, compressed to the region's short line.
    Map,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Record => "record",
            Level::Index => "index",
            Level::Map => "map",
        }
    }
}

/// One addressable region of a space. The caller keeps the bodies; a region is
/// only what the renderer needs to choose a scale.
pub struct Region {
    /// Where the region lives: an event id, a node record entry, a path.
    pub handle: String,
    /// One line that stands for the region at index scale.
    pub line: String,
    /// Serialized size of the body, so the renderer measures without allocating.
    pub bytes: usize,
}

impl Region {
    pub fn new(handle: impl Into<String>, line: impl Into<String>, bytes: usize) -> Self {
        Self {
            handle: handle.into(),
            line: line.into(),
            bytes,
        }
    }
}

/// A space rendered within a budget: the head whole, the rest listed, the
/// oldest folded into one address line.
pub struct View {
    /// Indices of the regions kept whole, in order.
    pub record: Vec<usize>,
    pub index: Vec<(String, String)>,
    pub map: Vec<(String, String)>,
    /// The scale the last region landed at.
    pub level: Level,
    /// Size of everything the view shows.
    pub bytes: usize,
}

/// How much of one tool result a transcript carries. The record keeps the whole
/// result; the node acts on a view that names the entry that unfolds it.
pub const TOOL_BYTES: usize = 8 * 1024;

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn suffix(text: &str, bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Every result carries a stable address, including results that fit whole. The
/// complete result is written once, in the host's immutable record.
pub fn tool_result(value: &serde_json::Value, node: u64, entry: usize) -> serde_json::Value {
    use serde_json::json;
    let reference = json!({"node": node, "entry": entry});
    let whole = json!({"record": reference, "result": value});
    let text = value.to_string();
    if whole.to_string().len() <= TOOL_BYTES {
        return whole;
    }
    let mut payload = TOOL_BYTES / 2;
    loop {
        let view = json!({"record": reference, "truncated": true, "bytes": text.len(),
            "note": "the full result stays in the record; use eja_read_record",
            "head": prefix(&text, payload * 2 / 3), "tail": suffix(&text, payload / 3)});
        if view.to_string().len() <= TOOL_BYTES {
            return view;
        }
        payload /= 2;
    }
}

/// Fold a live transcript to its budget. The head (system and assignment) and
/// the newest steps stay whole; older steps become one address line each; the
/// oldest fold into a single address. Returns false when even that does not fit,
/// which is the node's stopping point, not a licence to keep re-sending at the cap.
pub(crate) fn transcript(messages: &mut Vec<Value>, budget: usize) -> bool {
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
    let regions: Vec<Region> = steps
        .iter()
        .zip(&bodies)
        .enumerate()
        .rev()
        .map(|(position, ((start, end), body))| {
            let bytes = body.iter().map(|m| m.to_string().len()).sum::<usize>();
            Region::new(
                format!("step:{}", position + 1),
                step_line(*start, *end, &messages[*start..*end]),
                bytes,
            )
        })
        .collect();
    let head_bytes: usize = messages[..head].iter().map(|m| m.to_string().len()).sum();
    let view = assemble(&regions, budget.saturating_sub(head_bytes + 8));
    let kept: Vec<usize> = view.record.iter().map(|i| steps.len() - 1 - *i).collect();
    let tail = tail(&view, "Earlier steps of this node, folded:");
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
    if !transcript(&mut messages, limits.context_bytes) {
        return Err(ContextBudget.into());
    }
    Ok(messages)
}

fn line_bytes(line: &str) -> usize {
    line.len() + 8
}

fn footer(record: usize, index: usize, map: usize, level: Level) -> String {
    format!(
        "\n[view {record} whole, {index} listed, {map} folded — landed at {}; unfold by handle]\n",
        level.name()
    )
}

/// Room reserved for the fold line and the footer when the tail collapses.
const TAIL: usize = 160;
/// Room reserved so the rendered text's footer always fits inside the budget.
const FOOTER: usize = 80;

/// Fold `regions` (highest priority first) into a view no larger than `budget`.
///
/// Three scales, chosen by the budget: whole bodies for the head while they fit,
/// one line each for the rest, and — when even the lines do not fit — one address
/// range for the tail. The result is deterministic and fits the budget, except
/// for a budget smaller than the fold line itself.
pub fn assemble(regions: &[Region], budget: usize) -> View {
    let count = regions.len();
    if count == 0 {
        return build(regions, Vec::new(), Vec::new(), 0, budget);
    }
    let room = |extra: usize| budget.saturating_sub(FOOTER + extra);

    let whole: usize = regions.iter().map(|region| region.bytes + 8).sum();
    if whole <= room(0) {
        return build(
            regions,
            vec![Level::Record; count],
            Vec::new(),
            count,
            budget,
        );
    }
    let listed: usize = regions.iter().map(|region| line_bytes(&region.line)).sum();
    if listed <= room(0) {
        let mut widths = vec![Level::Index; count];
        let mut left = room(0) - listed;
        for (index, region) in regions.iter().enumerate() {
            if region.bytes + 8 > left {
                break;
            }
            left -= region.bytes + 8;
            widths[index] = Level::Record;
        }
        return build(regions, widths, Vec::new(), count, budget);
    }
    // Not even one line each: keep the head's lines and fold the tail into one
    // address range, so a space of any size still states where it went.
    let available = budget.saturating_sub(FOOTER + TAIL);
    let mut widths = vec![Level::Map; count];
    let mut used = 0usize;
    let mut kept = 0usize;
    for region in regions {
        if used + line_bytes(&region.line) > available {
            break;
        }
        used += line_bytes(&region.line);
        widths[kept] = Level::Index;
        kept += 1;
    }
    let mut map = Vec::new();
    if kept < count {
        map.push((
            String::new(),
            format!(
                "  … {} older region(s), {} .. {} — unfold by handle",
                count - kept,
                regions[kept].handle,
                regions[count - 1].handle
            ),
        ));
    }
    build(regions, widths, map, kept, budget)
}

fn build(
    regions: &[Region],
    widths: Vec<Level>,
    map: Vec<(String, String)>,
    shown: usize,
    budget: usize,
) -> View {
    let mut record = Vec::new();
    let mut index = Vec::new();
    let mut bytes = 0usize;
    for (position, (region, width)) in regions.iter().zip(widths.iter()).take(shown).enumerate() {
        match width {
            Level::Record => {
                bytes += region.bytes + 8;
                record.push(position);
            }
            Level::Index => {
                bytes += line_bytes(&region.line);
                index.push((region.handle.clone(), region.line.clone()));
            }
            Level::Map => {}
        }
    }
    bytes += map.iter().map(|(_, line)| line_bytes(line)).sum::<usize>();
    let level = widths.last().copied().unwrap_or(Level::Map);
    // The declared budget is the contract. The only irreducible content is the
    // single fold line, so a budget smaller than that cannot be met by any view.
    debug_assert!(bytes <= budget.max(TAIL) || budget == usize::MAX);
    View {
        record,
        index,
        map,
        level,
        bytes,
    }
}

/// Render a view as text within its declared budget.
pub fn text_within(view: &View, bodies: &[String], title: &str, budget: usize) -> String {
    let rendered = text(view, bodies, title);
    if rendered.len() <= budget {
        return rendered;
    }
    // Degenerate budgets cannot hold even the footer; cut at a character edge.
    let mut end = budget.min(rendered.len());
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    rendered[..end].to_string()
}

/// Render a view as text: whole bodies first, then lines, then folded addresses.
pub fn text(view: &View, bodies: &[String], title: &str) -> String {
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(title);
        out.push('\n');
    }
    for index in &view.record {
        let body = bodies.get(*index).map_or("", String::as_str);
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    for (_, line) in &view.index {
        out.push_str(line);
        out.push('\n');
    }
    for (_, line) in &view.map {
        out.push_str(line);
        out.push('\n');
    }
    // A whole view needs no notice: the landing scale matters only when the
    // budget forced something out.
    if view.index.is_empty() && view.map.is_empty() {
        return out;
    }
    out.push_str(&footer(
        view.record.len(),
        view.index.len(),
        view.map.len(),
        view.level,
    ));
    out
}

/// The folded part of a transcript: one address line per region that did not
/// stay whole.
pub fn tail(view: &View, title: &str) -> Option<String> {
    if view.index.is_empty() && view.map.is_empty() {
        return None;
    }
    let mut out = String::from(title);
    out.push('\n');
    for (_, line) in &view.index {
        out.push_str(line);
        out.push('\n');
    }
    for (_, line) in &view.map {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&footer(
        view.record.len(),
        view.index.len(),
        view.map.len(),
        view.level,
    ));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(count: usize, body: usize) -> Vec<Region> {
        (0..count)
            .map(|index| Region::new(format!("event:{index}"), format!("  #{index} line"), body))
            .collect()
    }

    #[test]
    fn a_space_that_fits_is_whole() {
        let view = assemble(&regions(3, 10), 10_000);
        assert_eq!(view.level, Level::Record);
        assert_eq!(view.record.len(), 3);
        assert!(view.index.is_empty() && view.map.is_empty());
    }

    #[test]
    fn the_head_stays_whole_and_the_tail_is_listed() {
        let view = assemble(&regions(5, 900), 3_000);
        assert!(view.record.len() >= 2, "the head stays whole");
        assert_eq!(view.record.len() + view.index.len(), 5);
        assert_eq!(view.level, Level::Index);
        assert_eq!(view.record[0], 0);
    }

    #[test]
    fn when_even_lines_do_not_fit_the_tail_folds_into_one_address() {
        let view = assemble(&regions(200, 10), 900);
        assert_eq!(view.map.len(), 1);
        assert_eq!(view.level, Level::Map);
        assert!(view.map[0].1.contains("unfold by handle"));
    }

    #[test]
    fn a_view_never_exceeds_its_budget() {
        for budget in [200usize, 500, 1_000, 4_000] {
            for count in [1usize, 3, 40, 400] {
                let view = assemble(&regions(count, 64), budget);
                assert!(
                    view.bytes <= budget,
                    "count={count} budget={budget} bytes={}",
                    view.bytes
                );
            }
        }
    }

    #[test]
    fn the_rendered_text_states_its_landing_scale() {
        let view = assemble(&regions(6, 500), 2_000);
        let bodies: Vec<String> = (0..6).map(|_| "body".to_string()).collect();
        let rendered = text(&view, &bodies, "SPACE");
        assert!(rendered.starts_with("SPACE\n"));
        assert!(rendered.contains("[view "));
        assert!(rendered.contains("unfold by handle"));
    }
}
