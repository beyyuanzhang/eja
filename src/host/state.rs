//! The host reads the journal and serializes the one core fold.
//!
//! There is no second fold here: `eja::State` is the only derivation of the
//! log, and this module only reads the raw lines (tolerating a torn tail) and
//! renders that one state as the API's JSON. Semantics live in the core.

use eja::{
    Event, Kind, OpenProposal, ProposalKind, RelationKind, State, Task, TaskKind, TaskStatus,
};
use serde_json::{json, Value};
use std::{fs, path::Path};

/// The view's local encounter bookmark. The runtime owns every derivation;
/// the bookmark only says which exchange this view is looking at.
#[derive(Clone, Copy, Default)]
pub struct Encounter {
    pub anchor: u64,
    pub task: Option<u64>,
    pub proposal: Option<u64>,
}

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
pub fn state_json(
    journal: &Journal,
    health: &Health,
    busy: bool,
    encounter: Option<Encounter>,
) -> Value {
    let state = State::fold(journal.events.clone());
    let events: Vec<Value> = journal
        .events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    let tasks: Vec<Value> = state
        .tasks()
        .iter()
        .map(|task| task_json(&state, task))
        .collect();
    let mut view = json!({
        "events": events,
        "tasks": &tasks,
        "lastEvent": state.last_id(),
        "health": health,
        "presence": presence_voice(&state),
    });
    let loops = open_loops(&state, &tasks, busy);
    let loops = match encounter {
        None => loops,
        Some(bookmark) => loops
            .into_iter()
            .filter(|item| in_encounter(item, &state, bookmark))
            .collect(),
    };
    view["loops"] = Value::Array(loops);
    view["spoken"] = encounter.map_or_else(
        || Value::Array(Vec::new()),
        |bookmark| spoken(&state, bookmark),
    );
    view
}

/// The exchange an encounter shows: what was said since its bookmark, without
/// eja's inner voice. An eja message belongs to the conversation only when the
/// user was present in its turn.
fn spoken(state: &State, bookmark: Encounter) -> Value {
    let present: Vec<u64> = state
        .events()
        .iter()
        .filter(|event| {
            matches!(event.kind, Kind::User | Kind::Returned | Kind::Denial)
                || (event.kind == Kind::Approval
                    && event
                        .data
                        .as_ref()
                        .and_then(|data| data.get("source"))
                        .and_then(Value::as_str)
                        != Some("eja"))
        })
        .map(|event| event.turn)
        .collect();
    Value::Array(
        state
            .events()
            .iter()
            .filter(|event| event.id > bookmark.anchor)
            .filter(|event| matches!(event.kind, Kind::User | Kind::Eja))
            .filter(|event| event.kind == Kind::User || present.contains(&event.turn))
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
            .collect(),
    )
}

/// The return screen's voice: eja's own reply to the latest return, spoken
/// only if it chose to speak on that turn. A later departure or alone turn
/// means the user left; the voice rests until the next return.
fn presence_voice(state: &State) -> Value {
    let latest = state
        .events()
        .iter()
        .rev()
        .find(|event| matches!(event.kind, Kind::Returned | Kind::Departed | Kind::Alone));
    let Some(latest) = latest.filter(|event| event.kind == Kind::Returned) else {
        return Value::Null;
    };
    state
        .events()
        .iter()
        .find(|event| {
            event.kind == Kind::Eja && event.turn == latest.turn && !event.text.trim().is_empty()
        })
        .map_or(Value::Null, |event| {
            json!({
                "id": event.id,
                "role": "eja",
                "text": event.text,
                "at": event.at,
                "turn": event.turn,
                "data": event.data,
            })
        })
}

fn proposal_json(proposal: &OpenProposal) -> Value {
    json!({
        "id": proposal.id,
        "kind": proposal.data.kind,
        "target_id": proposal.data.target_id,
        "text": proposal.text,
        "at": proposal.at,
        "turn": proposal.turn,
    })
}

fn loop_title(text: &str, id: u64) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(104).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else if head.is_empty() {
        format!("#{id}")
    } else {
        head
    }
}

fn task_loop(task: &Task, json: &Value, kind: &str) -> Value {
    json!({
        "key": format!("task:{}", task.id),
        "kind": kind,
        "eventId": task.event_ids.iter().max().copied().unwrap_or(task.id),
        "at": task.reported_at.or(task.closed_at).or(task.assigned_at).unwrap_or(task.created_at),
        "title": loop_title(&task.title, task.id),
        "task": json,
    })
}

/// A rejection or denial already answers the question; a correction withdraws
/// rather than asks. Proposal ids are never re-asked.
fn answered(proposal: &OpenProposal, state: &State) -> bool {
    if proposal.data.kind == ProposalKind::Correction {
        return true;
    }
    state.events().iter().any(|event| {
        matches!(event.kind, Kind::ProposalRejected | Kind::Denial)
            && event
                .data
                .as_ref()
                .and_then(|data| data.get("proposal_id"))
                .and_then(Value::as_u64)
                == Some(proposal.id)
    })
}

fn group_review(task: &Task, children: &[u64], tasks: &[Task]) -> bool {
    task.kind == TaskKind::Group
        && task.status == TaskStatus::Open
        && !children.is_empty()
        && children.iter().all(|id| {
            tasks
                .iter()
                .find(|child| child.id == *id)
                .is_some_and(|child| matches!(child.status, TaskStatus::Done | TaskStatus::Failed))
        })
}

/// What still asks for the user: interrupted runs first, then decisions, then
/// reviews; newest first inside a rank.
fn open_loops(state: &State, tasks: &[Value], busy: bool) -> Vec<Value> {
    let mut loops: Vec<(u8, i64, Value)> = Vec::new();
    for proposal in state.proposals() {
        if answered(proposal, state) {
            continue;
        }
        loops.push((
            1,
            proposal.at,
            json!({
                "key": format!("proposal:{}", proposal.id),
                "kind": "decision",
                "eventId": proposal.id,
                "at": proposal.at,
                "title": loop_title(&proposal.text, proposal.id),
                "proposal": proposal_json(proposal),
            }),
        ));
    }
    for (task, json) in state.tasks().iter().zip(tasks) {
        let kind = if task.kind == TaskKind::Aja && task.status == TaskStatus::Waiting {
            Some(("review", 2))
        } else if task.status == TaskStatus::Running && !busy {
            Some(("interrupted", 0))
        } else if group_review(task, &children_of(state, task.id), state.tasks()) {
            Some(("review", 2))
        } else {
            None
        };
        if let Some((kind, rank)) = kind {
            let item = task_loop(task, json, kind);
            let at = item["at"].as_i64().unwrap_or_default();
            loops.push((rank, at, item));
        }
    }
    loops.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    loops.into_iter().map(|(_, _, item)| item).collect()
}

fn in_encounter(item: &Value, state: &State, bookmark: Encounter) -> bool {
    let boundary = state
        .events()
        .iter()
        .rev()
        .find(|event| event.kind == Kind::User && event.id > bookmark.anchor)
        .map_or(bookmark.anchor, |event| event.id);
    if item["eventId"].as_u64().unwrap_or_default() > boundary {
        return true;
    }
    match item["kind"].as_str() {
        Some("decision") => item["proposal"]["id"].as_u64() == bookmark.proposal,
        _ => item["task"]["id"].as_u64() == bookmark.task,
    }
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
    use super::{journal_task, state_json, Encounter, Health, Journal};
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
        let state = state_json(&journal, &health(), false, None);

        assert_eq!(
            state["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| matches!(event["kind"].as_str(), Some("user" | "eja")))
                .count(),
            2
        );
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
    fn the_return_voice_rests_after_a_departure() {
        let journal = Journal {
            events: vec![
                event(1, 1, "eja", Kind::Returned, "", json!({})),
                event(2, 1, "eja", Kind::Eja, "welcome back", json!({})),
                event(3, 2, "eja", Kind::Departed, "", json!({})),
            ],
            error: None,
            torn: false,
        };
        let state = state_json(&journal, &health(), false, None);
        assert_eq!(state["presence"], Value::Null);
        let journal = Journal {
            events: vec![
                event(1, 1, "eja", Kind::Returned, "", json!({})),
                event(2, 1, "eja", Kind::Eja, "welcome back", json!({})),
            ],
            error: None,
            torn: false,
        };
        let state = state_json(&journal, &health(), false, None);
        assert_eq!(state["presence"]["text"], "welcome back");
    }

    #[test]
    fn loops_answer_once_and_sort_by_need() {
        let journal = Journal {
            events: vec![
                event(1, 1, "eja", Kind::User, "do it", json!({})),
                event(
                    2,
                    1,
                    "eja",
                    Kind::Proposal,
                    "may I rename you",
                    json!({"kind": "commitment", "target_id": null}),
                ),
                event(3, 2, "eja", Kind::Denial, "", json!({"proposal_id": 2})),
                event(
                    4,
                    3,
                    "eja",
                    Kind::Proposal,
                    "a second question",
                    json!({"kind": "goal", "target_id": null}),
                ),
                event(
                    5,
                    3,
                    "task:7",
                    Kind::TaskOpened,
                    "build",
                    json!({"kind": "aja", "title": "build", "request": "r", "origin": "user", "acceptance": "a"}),
                ),
                event(
                    6,
                    3,
                    "task:7",
                    Kind::TaskAssigned,
                    "",
                    json!({"task_id": 7}),
                ),
                event(7, 3, "task:7", Kind::WorkerReport, "done", json!({})),
            ],
            error: None,
            torn: false,
        };
        let state = state_json(&journal, &health(), false, None);
        let loops = state["loops"].as_array().unwrap();
        assert_eq!(loops.len(), 2, "{loops:?}");
        assert_eq!(loops[0]["kind"], "decision");
        assert_eq!(loops[0]["proposal"]["id"], 4);
        assert_eq!(loops[1]["kind"], "review");
        // A running task with no turn in flight is an interrupted loop.
        let running = Journal {
            events: journal
                .events
                .iter()
                .filter(|event| event.id != 7)
                .cloned()
                .collect(),
            error: None,
            torn: false,
        };
        let state = state_json(&running, &health(), false, None);
        let loops = state["loops"].as_array().unwrap();
        assert_eq!(loops[0]["kind"], "interrupted");
        let state = state_json(&running, &health(), true, None);
        assert!(state["loops"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] != "interrupted"));
        // The encounter bookmark keeps its own decision and newer events.
        let state = state_json(
            &journal,
            &health(),
            false,
            Some(Encounter {
                anchor: 3,
                task: None,
                proposal: Some(4),
            }),
        );
        let loops = state["loops"].as_array().unwrap();
        assert!(loops.iter().any(|item| item["proposal"]["id"] == 4));
        assert!(loops.iter().any(|item| item["kind"] == "review"));
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
