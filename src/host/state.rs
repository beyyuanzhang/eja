//! The host reads the journal and serializes the one core fold.
//!
//! There is no second fold here: `eja::State` is the only derivation of the
//! log, and this module only reads the raw lines (tolerating a torn tail) and
//! renders that one state as the API's JSON. Semantics live in the core.

use eja::{Event, Kind, RelationKind, State, Task};
use serde_json::{json, Value};
use std::{fs, path::Path};

pub struct Journal {
    pub events: Vec<Event>,
    pub error: Option<String>,
    pub torn: bool,
}

impl Journal {
    pub fn read(dir: &Path) -> Self {
        let path = dir.join("log.jsonl");
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    events: Vec::new(),
                    error: None,
                    torn: false,
                };
            }
            Err(error) => {
                return Self {
                    events: Vec::new(),
                    error: Some(error.to_string()),
                    torn: false,
                };
            }
        };
        let ended = raw.ends_with('\n');
        let mut lines: Vec<&str> = raw.split('\n').collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        let mut events = Vec::new();
        let mut errors = Vec::new();
        let mut torn = false;
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(event) => events.push(event),
                Err(error) => {
                    if index + 1 == lines.len() && !ended {
                        torn = true;
                        break;
                    }
                    errors.push(format!("line {}: {error}", index + 1));
                }
            }
        }
        Self {
            events,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            torn,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Health {
    pub model: bool,
    pub model_name: String,
    pub state: bool,
    pub mcp: bool,
    pub journal: &'static str,
    pub journal_error: Option<String>,
    pub torn: bool,
    pub turn_in_flight: bool,
}

impl serde::Serialize for Health {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Health", 8)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("modelName", &self.model_name)?;
        state.serialize_field("state", &self.state)?;
        state.serialize_field("mcp", &self.mcp)?;
        state.serialize_field("journal", &self.journal)?;
        state.serialize_field("journalError", &self.journal_error)?;
        state.serialize_field("torn", &self.torn)?;
        state.serialize_field("turnInFlight", &self.turn_in_flight)?;
        state.end()
    }
}

fn children_of(state: &State, id: u64) -> Vec<u64> {
    state
        .relations()
        .iter()
        .filter(|edge| edge.kind == RelationKind::PartOf && edge.to == id)
        .map(|edge| edge.from)
        .collect()
}

fn task_json(state: &State, task: &Task) -> Value {
    let run = task.run.as_ref();
    json!({
        "id": task.id,
        "kind": task.kind,
        "title": task.title,
        "request": task.request,
        "origin": task.origin,
        "acceptance": task.acceptance,
        "workspace": task.workspace,
        "status": task.status.as_str(),
        "created_at": task.created_at,
        "assigned_at": task.assigned_at,
        "reported_at": task.reported_at,
        "closed_at": task.closed_at,
        "close": task.close,
        "run": {
            "digest": run.and_then(|run| run.digest.clone()),
            "evidence": run.and_then(|run| run.evidence.clone()).unwrap_or(Value::Null),
            "evidence_id": run.and_then(|run| run.evidence_id),
            "evidence_result": run.and_then(|run| run.evidence_result.clone()).unwrap_or(Value::Null),
            "witness_at": run.and_then(|run| run.witness_at),
            "reconcile": run.and_then(|run| run.reconcile.clone()).unwrap_or(Value::Null),
        },
        "event_ids": task.event_ids,
        "children": children_of(state, task.id),
    })
}

/// The API state: one core fold, serialized. Nothing here re-derives meaning.
pub fn state_json(journal: &Journal, health: &Health) -> Value {
    let state = State::fold(journal.events.clone());
    let events: Vec<Value> = journal
        .events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    let messages: Vec<Value> = state
        .events()
        .iter()
        .filter(|event| matches!(event.kind, Kind::User | Kind::Eja))
        .map(|event| {
            json!({
                "id": event.id,
                "role": event.kind.as_str(),
                "text": event.text,
                "at": event.at,
                "turn": event.turn,
                "data": event.data,
            })
        })
        .collect();
    let tasks: Vec<Value> = state
        .tasks()
        .iter()
        .map(|task| task_json(&state, task))
        .collect();
    let proposals: Vec<Value> = state
        .proposals()
        .iter()
        .map(|proposal| {
            json!({
                "id": proposal.id,
                "kind": proposal.data.kind,
                "target_id": proposal.data.target_id,
                "text": proposal.text,
                "at": proposal.at,
                "turn": proposal.turn,
            })
        })
        .collect();
    json!({
        "events": events,
        "messages": messages,
        "tasks": tasks,
        "proposals": proposals,
        "lastEvent": state.last_id(),
        "health": health,
    })
}

pub fn journal_event(journal: &Journal, id: u64) -> Option<Value> {
    journal
        .events
        .iter()
        .find(|event| event.id == id)
        .and_then(|event| serde_json::to_value(event).ok())
}

pub fn journal_task(journal: &Journal, id: u64) -> Option<Value> {
    let state = State::fold(journal.events.clone());
    let task = state.task(id)?;
    let events: Vec<Value> = journal
        .events
        .iter()
        .filter(|event| task.event_ids.contains(&event.id))
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    Some(json!({ "task": task_json(&state, task), "events": events }))
}

#[cfg(test)]
mod tests {
    use super::{journal_task, state_json, Health, Journal};
    use eja::{Event, Kind};
    use serde_json::{json, Value};

    fn event(id: u64, turn: u64, scope: &str, kind: Kind, text: &str, data: Value) -> Event {
        Event {
            id,
            at: 1_700_000_000 + i64::try_from(id).unwrap_or_default(),
            turn,
            scope: scope.to_owned(),
            kind,
            text: text.to_owned(),
            closes: None,
            data: Some(data),
        }
    }

    fn health() -> Health {
        Health {
            model: true,
            model_name: "model".to_owned(),
            state: true,
            mcp: false,
            journal: "ok",
            journal_error: None,
            torn: false,
            turn_in_flight: false,
        }
    }

    #[test]
    fn the_api_serializes_the_one_core_fold() {
        let events = vec![
            event(1, 1, "eja", Kind::User, "group this", json!({})),
            event(
                2,
                1,
                "task:1",
                Kind::TaskOpened,
                "group",
                json!({
                    "kind": "group",
                    "title": "release",
                    "request": "ship it",
                    "origin": "user",
                    "acceptance": "tests pass"
                }),
            ),
            event(
                3,
                1,
                "task:2",
                Kind::TaskOpened,
                "child",
                json!({
                    "kind": "aja",
                    "title": "build",
                    "request": "cargo build",
                    "origin": "eja",
                    "acceptance": "exit zero",
                    "evidence": {"tool": "shell"}
                }),
            ),
            event(
                4,
                1,
                "eja",
                Kind::TaskRelation,
                "",
                json!({"from": 2, "to": 1, "kind": "part_of"}),
            ),
            event(
                5,
                1,
                "task:2",
                Kind::TaskAssigned,
                "",
                json!({"task_id": 2, "objective": "build", "input": "cargo build", "acceptance": "exit zero", "issued_at": 1}),
            ),
            event(6, 1, "task:2", Kind::WorkerReport, "built", json!({})),
            event(
                7,
                1,
                "task:2",
                Kind::Observed,
                "",
                json!({"kind": "witness", "target": "world:out", "arguments": {}, "result": {"ok": true}}),
            ),
            event(
                8,
                1,
                "task:2",
                Kind::TaskClosed,
                "",
                json!({"accepted": true, "note": "done"}),
            ),
            event(9, 1, "eja", Kind::Eja, "finished", json!({})),
        ];
        let journal = Journal {
            events,
            error: None,
            torn: false,
        };
        let state = state_json(&journal, &health());

        assert_eq!(state["messages"].as_array().unwrap().len(), 2);
        assert_eq!(state["events"].as_array().unwrap().len(), 9);
        assert_eq!(state["lastEvent"], 9);
        assert_eq!(state["tasks"][0]["children"], json!([2]));
        assert_eq!(state["tasks"][1]["status"], "done");
        assert_eq!(state["tasks"][1]["run"]["evidence_id"], 7);
        assert_eq!(
            state["tasks"][1]["run"]["evidence_result"],
            json!({"ok": true})
        );
        assert_eq!(state["tasks"][1]["close"]["accepted"], true);
        assert_eq!(state["tasks"][1]["event_ids"], json!([3, 5, 6, 7, 8]));

        let raw = journal_task(&journal, 2).expect("task");
        assert_eq!(raw["task"]["title"], "build");
        assert_eq!(raw["events"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn an_unfinished_last_line_is_torn_not_corruption() {
        let root = std::env::temp_dir().join(format!(
            "eja-host-state-{}-{}",
            std::process::id(),
            super::super::util::now_seconds()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let event = event(1, 1, "eja", Kind::User, "hello", json!({}));
        let line = serde_json::to_string(&event).unwrap();
        std::fs::write(root.join("log.jsonl"), format!("{line}\n{{\"id\":2")).unwrap();
        let journal = Journal::read(&root);
        assert_eq!(journal.events.len(), 1);
        assert!(journal.torn);
        assert!(journal.error.is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
