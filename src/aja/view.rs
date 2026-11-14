//! One bounded working view, shared by live execution and record replay.
use serde_json::{json, Value};

pub(crate) const TOOL_BYTES: usize = 8 * 1024;

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

/// Every result carries a stable address, including results that fit whole.
/// The complete result is written once, in the host's immutable record.
pub(crate) fn tool_result(value: &Value, node: u64, entry: usize) -> Value {
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
