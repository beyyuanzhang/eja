//! One renderer for every model-facing view.
//!
//! A view is a space of regions ordered by priority: the newest step first, the
//! current input first, the evidence before the index. The renderer keeps the
//! head whole while the declared budget allows, lists the rest as one line each,
//! and folds the oldest into one address range. Every region names the address
//! that reads it back, so folding is an address lookup, never a re-derivation:
//! the space is folded, not summarized.
//!
//! EJA's projection and a aja transcript are the same call with a different
//! payload. Nothing here writes state or calls a model.

use crate::aja::Limits;
use crate::{Assignment, Result};
use serde_json::{json, Value};

/// The scale a region is shown at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The region's whole body.
    Record,
    /// One line with the region's address.
    Index,
    /// One address range for the folded tail.
    Map,
}

impl Level {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Index => "index",
            Self::Map => "map",
        }
    }
}

/// One addressable region of a space. The caller keeps the bodies; a region is
/// only what the renderer needs to choose a scale and to name an address.
pub struct Region {
    /// Where the region lives: `log:event:12`, `log:task:3`, `record 3/7`.
    pub address: String,
    /// One line that stands for the region at index scale.
    pub line: String,
    /// Serialized size of the body, so the renderer measures without allocating.
    pub bytes: usize,
}

impl Region {
    pub(crate) fn new(address: impl Into<String>, line: impl Into<String>, bytes: usize) -> Self {
        Self {
            address: address.into(),
            line: line.into(),
            bytes,
        }
    }
}

/// A space rendered within a budget: the head whole, the rest listed, the
/// oldest folded into one address line. The texts stay in the regions.
pub struct View {
    /// Indices of the regions kept whole, in order.
    pub record: Vec<usize>,
    /// Indices of the regions listed as one line.
    pub index: Vec<usize>,
    /// The one line standing for everything older, when even lines do not fit.
    pub map: Option<String>,
    /// The scale the last region landed at.
    pub level: Level,
}

/// How much of one tool result a transcript carries. The record keeps the whole
/// result; the aja acts on a view that names the entry that unfolds it.
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
pub fn tool_result(value: &serde_json::Value, aja: u64, entry: usize) -> serde_json::Value {
    use serde_json::json;
    let reference = json!({"aja": aja, "entry": entry});
    let whole = json!({"record": reference, "result": value});
    let text = value.to_string();
    if crate::measure::value(&whole) <= TOOL_BYTES {
        return whole;
    }
    let mut payload = TOOL_BYTES / 2;
    loop {
        let view = json!({"record": reference, "truncated": true, "bytes": text.len(),
            "note": "the full result stays in the record; use eja_read_record",
            "head": prefix(&text, payload * 2 / 3), "tail": suffix(&text, payload / 3)});
        if crate::measure::value(&view) <= TOOL_BYTES {
            return view;
        }
        payload /= 2;
    }
}

/// Fold a live transcript to its budget. The head (system and assignment) and
/// the newest steps stay whole; older steps become one address line each; the
/// oldest fold into a single address. Returns false when even that does not fit,
/// which is the aja's stopping point, not a licence to keep re-sending at the cap.
pub fn transcript(messages: &mut Vec<Value>, budget: usize) -> bool {
    let size = |messages: &[Value]| messages.iter().map(crate::measure::value).sum::<usize>();
    // The common case: the space already fits, so nothing is measured twice.
    if size(messages) <= budget {
        return true;
    }
    let head = messages
        .iter()
        .take_while(|message| message["role"] == "system" || message["role"] == "user")
        .count()
        .min(2);
    if messages.len() <= head {
        // Nothing to fold: the assignment itself must fit.
        return size(messages) + 8 <= budget;
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
    let regions: Vec<Region> = steps
        .iter()
        .enumerate()
        .rev()
        .map(|(position, (start, end))| step_region(position + 1, &messages[*start..*end]))
        .collect();
    let head_bytes: usize = messages[..head].iter().map(crate::measure::value).sum();
    let view = assemble(&regions, budget.saturating_sub(head_bytes + 8));
    let mut kept: Vec<usize> = view.record.iter().map(|i| steps.len() - 1 - *i).collect();
    let tail = tail(&view, &regions, "Earlier steps of this aja, folded:");
    let mut rebuilt: Vec<Value> = messages[..head].to_vec();
    if let Some(text) = &tail {
        rebuilt.push(json!({"role":"user","content":text}));
    }
    kept.sort_unstable();
    for step in &kept {
        let (start, end) = steps[*step];
        rebuilt.extend_from_slice(&messages[start..end]);
    }
    *messages = rebuilt;
    size(messages) <= budget
}

/// One step as a region: its line names the record entry that unfolds it, and
/// that same entry is the region's address.
fn step_region(number: usize, messages: &[Value]) -> Region {
    let mut names: Vec<String> = Vec::new();
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
    let mut address = String::new();
    let mut snippet = String::new();
    for message in messages.iter().skip(1) {
        let content = message["content"].as_str().unwrap_or("");
        if address.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(content) {
                if let Some(record) = value.get("record") {
                    address = format!(
                        "{}/{}",
                        record["aja"].as_u64().unwrap_or(0),
                        record["entry"].as_u64().unwrap_or(0)
                    );
                }
            }
        }
        if snippet.is_empty() {
            snippet = content.replace(['\n', '\r'], " ");
            // Fold at a character boundary: a byte index is not a character index.
            snippet.truncate(snippet.floor_char_boundary(80));
        }
    }
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
    let bytes = messages.iter().map(crate::measure::value).sum();
    Region::new(
        address,
        format!("  step{number}: {action}{suffix} → {snippet}"),
        bytes,
    )
}

pub fn fresh_messages(assignment: &Assignment, limits: &Limits) -> Vec<Value> {
    let budget = limits.steps.map_or_else(
        || format!("You have {} seconds.", limits.timeout_ms / 1000),
        |steps| {
            format!(
                "You have at most {steps} model turns and {} seconds.",
                limits.timeout_ms / 1000
            )
        },
    );
    let system = format!("You are one AJA: a single bounded execution of the work assigned below. Complete it with the supplied tools. Your workspace is the current project; inherited messages describe earlier observations, not a restored world. {budget} Acceptance is the contract. Stop once it holds and report what you did. Tool results carry record addresses; use eja_read_record to read omitted passages. Never claim a failed tool succeeded.");
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
        brief = format!("Continuing as a new aja. {brief}");
    }
    vec![
        json!({"role":"system","content":system}),
        json!({"role":"user","content":brief}),
    ]
}

#[derive(Debug)]
pub struct ContextBudget;
impl std::fmt::Display for ContextBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AJA context budget exhausted: assignment does not fit")
    }
}
impl std::error::Error for ContextBudget {}

pub fn prepare_messages(
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

const fn line_bytes(line: &str) -> usize {
    line.len() + 8
}

fn footer(record: usize, index: usize, map: usize, level: Level) -> String {
    format!(
        "\n[view {record} whole, {index} listed, {map} folded — landed at {}; unfold by address]\n",
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
        return build(regions, &[], None, 0, budget);
    }
    let room = |extra: usize| budget.saturating_sub(FOOTER + extra);

    let whole: usize = regions.iter().map(|region| region.bytes + 8).sum();
    if whole <= room(0) {
        return build(regions, &vec![Level::Record; count], None, count, budget);
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
        return build(regions, &widths, None, count, budget);
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
    let map = (kept < count).then(|| fold_line(regions, kept, count));
    build(regions, &widths, map, kept, budget)
}

/// The one line that stands for the folded tail: the addresses that read it, or
/// the fact that this space names none because the region itself is derived.
fn fold_line(regions: &[Region], kept: usize, count: usize) -> String {
    let folded = &regions[kept..count];
    let first = folded.iter().find(|region| !region.address.is_empty());
    let last = folded
        .iter()
        .rev()
        .find(|region| !region.address.is_empty());
    match (first, last) {
        (Some(first), Some(last)) => format!(
            "  … {} older region(s), {} .. {} — unfold by address",
            count - kept,
            first.address,
            last.address
        ),
        _ => format!(
            "  … {} older region(s) — this view is derived; its facts keep their ids",
            count - kept
        ),
    }
}

fn build(
    regions: &[Region],
    widths: &[Level],
    map: Option<String>,
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
                index.push(position);
            }
            Level::Map => {}
        }
    }
    if let Some(line) = &map {
        bytes += line_bytes(line);
    }
    let level = widths.last().copied().unwrap_or(Level::Map);
    // The declared budget is the contract. The only irreducible content is the
    // single fold line, so a budget smaller than that cannot be met by any view.
    debug_assert!(bytes <= budget.max(TAIL) || budget == usize::MAX);
    View {
        record,
        index,
        map,
        level,
    }
}

/// Render a view as text within its declared budget.
pub fn text_within(
    view: &View,
    regions: &[Region],
    title: &str,
    budget: usize,
    body: &dyn Fn(usize) -> String,
) -> String {
    let rendered = text(view, regions, title, body);
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

/// Render a view as text: whole bodies first, then lines, then the fold line.
/// A body is produced on demand, so a region that the budget folded is never
/// rendered in full.
pub fn text(
    view: &View,
    regions: &[Region],
    title: &str,
    body: &dyn Fn(usize) -> String,
) -> String {
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(title);
        out.push('\n');
    }
    for index in &view.record {
        let body = body(*index);
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    for index in &view.index {
        out.push_str(&regions[*index].line);
        out.push('\n');
    }
    if let Some(line) = &view.map {
        out.push_str(line);
        out.push('\n');
    }
    // A whole view needs no notice: the landing scale matters only when the
    // budget forced something out.
    if view.index.is_empty() && view.map.is_none() {
        return out;
    }
    out.push_str(&footer(
        view.record.len(),
        view.index.len(),
        usize::from(view.map.is_some()),
        view.level,
    ));
    out
}

/// The folded part of a space: one address line per region that did not stay
/// whole, plus the fold line for the rest.
pub fn tail(view: &View, regions: &[Region], title: &str) -> Option<String> {
    if view.index.is_empty() && view.map.is_none() {
        return None;
    }
    let mut out = String::from(title);
    out.push('\n');
    for index in &view.index {
        out.push_str(&regions[*index].line);
        out.push('\n');
    }
    if let Some(line) = &view.map {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&footer(
        view.record.len(),
        view.index.len(),
        usize::from(view.map.is_some()),
        view.level,
    ));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(count: usize, body: usize) -> Vec<Region> {
        (0..count)
            .map(|index| {
                Region::new(
                    format!("log:event:{index}"),
                    format!("  #{index} line"),
                    body,
                )
            })
            .collect()
    }

    #[test]
    fn a_space_that_fits_is_whole() {
        let view = assemble(&regions(3, 10), 10_000);
        assert_eq!(view.level, Level::Record);
        assert_eq!(view.record.len(), 3);
        assert!(view.index.is_empty() && view.map.is_none());
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
    fn when_even_lines_do_not_fit_the_tail_folds_into_one_address_range() {
        let view = assemble(&regions(200, 10), 900);
        let line = view.map.as_deref().expect("a fold line");
        assert_eq!(view.level, Level::Map);
        assert!(line.contains("unfold by address"), "{line}");
        // The range names the addresses that read the folded regions back.
        assert!(line.contains("log:event:"), "{line}");
    }

    #[test]
    fn a_rendered_view_never_exceeds_its_budget() {
        for budget in [200usize, 500, 1_000, 4_000] {
            for count in [1usize, 3, 40, 400] {
                let space = regions(count, 64);
                let view = assemble(&space, budget);
                let bodies: Vec<String> = (0..count).map(|_| "x".repeat(64)).collect();
                let rendered = text(&view, &space, "", &|index| bodies[index].clone());
                assert!(
                    rendered.len() <= budget,
                    "count={count} budget={budget} rendered={}",
                    rendered.len()
                );
            }
        }
    }

    #[test]
    fn the_rendered_text_states_its_landing_scale() {
        let space = regions(6, 500);
        let view = assemble(&space, 2_000);
        let bodies: Vec<String> = (0..6).map(|_| "body".to_string()).collect();
        let rendered = text(&view, &space, "SPACE", &|index| bodies[index].clone());
        assert!(rendered.starts_with("SPACE\n"));
        assert!(rendered.contains("[view "));
    }
}
