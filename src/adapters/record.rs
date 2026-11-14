//! Immutable run records, their ancestry, and bounded working projections.
use crate::{fold, Control, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

pub(crate) const READ_TOOL: &str = "eja_read_record";
pub(crate) fn definition() -> Value {
    json!({"type":"function","function":{"name":READ_TOOL,"description":"Read a bounded byte passage of a full tool result in this node's record or an inherited record. Use the record node/entry address from a tool result. Offsets address UTF-8 bytes.","parameters":{"type":"object","properties":{"node":{"type":"integer"},"entry":{"type":"integer"},"offset":{"type":"integer","default":0},"bytes":{"type":"integer","default":1024}},"required":["node","entry"],"additionalProperties":false}}})
}

fn path(root: &Path, id: u64) -> Result<PathBuf> {
    if id == 0 {
        return Err("invalid record node".into());
    }
    super::tools::relative_file(root, &format!("{id}/context.jsonl"))
}
fn start(root: &Path, id: u64, control: &Control) -> Result<Value> {
    control.check()?;
    let line = BufReader::new(fs::File::open(path(root, id)?)?)
        .lines()
        .next()
        .ok_or("record has no start entry")??;
    let entry: Value = serde_json::from_str(&line)?;
    if entry["kind"] != "start"
        || !entry["messages"]
            .as_array()
            .is_some_and(|messages| !messages.is_empty())
    {
        return Err("record has no valid start entry".into());
    }
    Ok(entry)
}
/// New records store only a parent address and this run's start messages. Legacy
/// records already contain their whole starting view and form an ancestry root.
pub(crate) fn lineage(
    root: &Path,
    predecessor: Option<u64>,
    control: &Control,
) -> Result<Vec<u64>> {
    let mut chain = Vec::new();
    let mut next = predecessor;
    let mut seen = HashSet::new();
    while let Some(id) = next {
        control.check()?;
        if !seen.insert(id) {
            return Err("record ancestry cycle".into());
        }
        let manifest_path = super::tools::relative_file(root, &format!("{id}/node.json"))?;
        let manifest: Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
        if manifest["frozen"] != true {
            return Err("predecessor record is not frozen".into());
        }
        chain.push(id);
        let initial = start(root, id, control)?;
        next = if initial["view_version"] == 1 {
            manifest
                .pointer("/assignment/resume")
                .and_then(Value::as_u64)
        } else {
            None
        };
    }
    Ok(chain)
}

pub(crate) fn replay(
    root: &Path,
    chain: &[u64],
    budget: usize,
    control: &Control,
) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    for &node in chain.iter().rev() {
        let mut pending = VecDeque::new();
        let mut unfinished = None;
        for (index, line) in BufReader::new(fs::File::open(path(root, node)?)?)
            .lines()
            .enumerate()
        {
            control.check()?;
            let entry: Value = serde_json::from_str(&line?)?;
            match entry["kind"].as_str() {
                Some("start") => {
                    let fresh = entry["messages"]
                        .as_array()
                        .ok_or("record start has no messages")?;
                    if entry["view_version"] != 1 {
                        messages.clear();
                    }
                    messages.retain(|message: &Value| message["role"] != "system");
                    for message in fresh.iter().rev() {
                        if message["role"] == "system" {
                            messages.insert(0, message.clone());
                        }
                    }
                    messages.extend(
                        fresh
                            .iter()
                            .filter(|message| message["role"] != "system")
                            .cloned(),
                    );
                }
                Some("assistant") => {
                    if !pending.is_empty() {
                        return Err(
                            "record starts a message before completing its tool group".into()
                        );
                    }
                    let message = entry["message"].clone();
                    crate::aja::validate_assistant(&message)?;
                    if let Some(calls) = message["tool_calls"]
                        .as_array()
                        .filter(|calls| !calls.is_empty())
                    {
                        unfinished = Some(messages.len());
                        for call in calls {
                            pending.push_back(
                                call["id"]
                                    .as_str()
                                    .ok_or("record tool call lacks id")?
                                    .to_string(),
                            );
                        }
                    }
                    messages.push(message);
                }
                Some("tool") => {
                    let id = pending
                        .pop_front()
                        .ok_or("record has an unmatched tool result")?;
                    if entry
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|actual| !actual.is_empty() && actual != id)
                    {
                        return Err("record tool ids do not match".into());
                    }
                    messages.push(json!({"role":"tool","tool_call_id":id,"content":fold::tool_result(&entry["result"],node,index).to_string()}));
                    if pending.is_empty() {
                        unfinished = None;
                    }
                }
                _ => {}
            }
            if pending.is_empty() {
                fold::transcript(&mut messages, budget);
            }
        }
        // Keep the complete prefix of an interrupted call group. It may have had
        // effects; its full, partial record remains separately inspectable.
        if let Some(index) = unfinished {
            messages.truncate(index);
        }
        fold::transcript(&mut messages, budget);
    }
    Ok(messages)
}

pub(crate) fn read(
    root: &Path,
    current: u64,
    ancestors: &[u64],
    arguments: &Value,
    control: &Control,
) -> Result<Value> {
    let node = arguments["node"]
        .as_u64()
        .ok_or("record node is required")?;
    if node != current && !ancestors.contains(&node) {
        return Err("record is outside this node's ancestry".into());
    }
    let wanted = arguments["entry"]
        .as_u64()
        .ok_or("record entry is required")?;
    for (index, line) in BufReader::new(fs::File::open(path(root, node)?)?)
        .lines()
        .enumerate()
    {
        control.check()?;
        let line = line?;
        if index as u64 != wanted {
            continue;
        }
        let entry: Value = serde_json::from_str(&line)?;
        if entry["kind"] != "tool" {
            return Err("record entry is not a tool result".into());
        }
        let text = entry["result"].to_string();
        let mut page = super::text_page(
            &text,
            arguments["offset"]
                .as_u64()
                .unwrap_or(0)
                .min(usize::MAX as u64) as usize,
            arguments["bytes"]
                .as_u64()
                .unwrap_or(1024)
                .min(usize::MAX as u64) as usize,
            1024,
        );
        page["node"] = json!(node);
        page["entry"] = json!(wanted);
        return Ok(page);
    }
    Err("record entry does not exist".into())
}
