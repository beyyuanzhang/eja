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
    /// The shortest line that still reports the region exists, at map scale.
    pub short: String,
    /// Serialized size of the body, so the renderer measures without allocating.
    pub bytes: usize,
}

impl Region {
    pub fn new(handle: impl Into<String>, line: impl Into<String>, bytes: usize) -> Self {
        let line = line.into();
        Self {
            handle: handle.into(),
            short: line.clone(),
            line,
            bytes,
        }
    }

    /// A shorter line for map scale: a region folded this far still reports
    /// that it exists and how to reach it.
    pub fn short(mut self, short: impl Into<String>) -> Self {
        let short = short.into();
        if short.len() < self.line.len() {
            self.short = short;
        }
        self
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

fn line_bytes(line: &str) -> usize {
    line.len() + 8
}

fn footer(record: usize, index: usize, map: usize) -> String {
    format!("\n[view {record} whole, {index} listed, {map} folded — unfold by handle]\n")
}

/// Room reserved for the fold line and the footer when the tail collapses.
const TAIL: usize = 160;
/// Room reserved so the rendered text's footer always fits inside the budget.
const FOOTER: usize = 80;

/// Fold `regions` (highest priority first) into a view no larger than `budget`.
///
/// Three scales, chosen by the budget: whole bodies for the head while they fit,
/// one line each for the rest, and the shortest line for everything that still
/// does not fit. Only when even the short lines exceed the budget does the tail
/// collapse into one address range. The result is deterministic and fits.
pub fn assemble(regions: &[Region], budget: usize) -> View {
    let count = regions.len();
    if count == 0 {
        return View {
            record: Vec::new(),
            index: Vec::new(),
            map: Vec::new(),
            level: Level::Map,
            bytes: 0,
        };
    }
    let room = |extra: usize| budget.saturating_sub(FOOTER + extra);

    let whole: usize = regions.iter().map(|region| region.bytes + 8).sum();
    if whole <= room(0) {
        return build(regions, vec![Level::Record; count], Vec::new(), count);
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
        return build(regions, widths, Vec::new(), count);
    }
    // Short lines: the head keeps its full line, the tail its short one.
    let shorts: usize = regions.iter().map(|region| line_bytes(&region.short)).sum();
    let reserve = if shorts <= room(TAIL) { 0 } else { TAIL };
    let available = room(reserve);
    let mut widths = vec![Level::Map; count];
    let mut used = 0usize;
    let mut kept = 0usize;
    for (index, region) in regions.iter().enumerate() {
        if used + line_bytes(&region.short) > available {
            break;
        }
        used += line_bytes(&region.short);
        widths[index] = Level::Map;
        kept += 1;
    }
    let mut left = available - used;
    for (index, region) in regions.iter().enumerate().take(kept) {
        if left < line_bytes(&region.line) {
            break;
        }
        left -= line_bytes(&region.line);
        widths[index] = Level::Index;
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
    build(regions, widths, map, kept)
}

fn build(regions: &[Region], widths: Vec<Level>, map: Vec<(String, String)>, shown: usize) -> View {
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
            Level::Map => {
                bytes += line_bytes(&region.short);
                index.push((region.handle.clone(), region.short.clone()));
            }
        }
    }
    bytes += map.iter().map(|(_, line)| line_bytes(line)).sum::<usize>();
    View {
        record,
        index,
        map,
        level: widths.last().copied().unwrap_or(Level::Map),
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
    out.push_str(&footer(view.record.len(), view.index.len(), view.map.len()));
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
    out.push_str(&footer(view.record.len(), view.index.len(), view.map.len()));
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
