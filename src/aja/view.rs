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
pub(crate) fn size(messages: &[Value]) -> usize {
    serde_json::to_vec(messages).map_or(usize::MAX, |bytes| bytes.len())
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

/// Preserve the current system and assignment, and complete provider tool-call
/// groups. Payload elision and removal only affect the view, never the record.
pub(crate) fn fit(messages: &mut Vec<Value>, budget: usize) -> bool {
    if size(messages) <= budget {
        return true;
    }
    let tool_indices: Vec<_> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message["role"] == "tool")
        .map(|(index, _)| index)
        .collect();
    for index in tool_indices {
        let content = messages[index]["content"].as_str().unwrap_or("");
        let value = serde_json::from_str::<Value>(content).unwrap_or(Value::Null);
        let marker = json!({"elided": true, "record": value.get("record"), "bytes": content.len()})
            .to_string();
        if marker.len() < content.len() {
            messages[index]["content"] = json!(marker);
        }
        if size(messages) <= budget {
            return true;
        }
    }
    loop {
        let current = messages
            .iter()
            .rposition(|message| message["role"] == "user");
        let candidate = (0..messages.len())
            .find(|index| messages[*index]["role"] != "system" && Some(*index) != current);
        let Some(start) = candidate else {
            return size(messages) <= budget;
        };
        let mut end = start + 1;
        if messages[start]["role"] == "assistant" {
            while end < messages.len() && messages[end]["role"] == "tool" {
                end += 1;
            }
        }
        messages.drain(start..end);
        if size(messages) <= budget {
            let notice = json!({"role":"user", "content":"Earlier context was elided to fit the working budget; complete evidence remains available by record address."});
            if size(messages).saturating_add(notice.to_string().len() + 1) <= budget {
                messages.insert(1.min(messages.len()), notice);
            }
            return true;
        }
    }
}
