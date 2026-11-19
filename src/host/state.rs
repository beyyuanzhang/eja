//! The host reads the journal and serializes the one core fold.
//!
//! There is no second fold here: `eja::State` is the only derivation of the
//! log, and this module only reads the raw lines (tolerating a torn tail) and
//! renders that one state as the API's JSON. Semantics live in the core.

use eja::{
    inspect_journal, Event, Kind, OpenProposal, ProposalKind, RelationKind, State, Task, TaskKind,
    TaskStatus,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    length: u64,
    modified: Option<SystemTime>,
}

/// A rebuildable derived view of the journal. The file remains the authority;
/// this cache only avoids folding and serializing every historical event on
/// every frontend poll.
pub struct Cache {
    dir: PathBuf,
    stamp: Option<Stamp>,
    journal: Journal,
    state: State,
    events: Vec<Value>,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            stamp: None,
            journal: Journal {
                events: Vec::new(),
                error: None,
                torn: false,
            },
            state: State::default(),
            events: Vec::new(),
        }
    }

    fn refresh(&mut self) {
        let stamp = fs::metadata(self.dir.join("log.jsonl"))
            .ok()
            .map(|metadata| Stamp {
                length: metadata.len(),
                modified: metadata.modified().ok(),
            });
        if self.stamp == stamp {
            return;
        }
        self.stamp = stamp;
        let mut journal = Journal::read(&self.dir);
        self.state = State::fold(std::mem::take(&mut journal.events));
        self.events = self
            .state
            .events()
            .iter()
            .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
            .collect();
        self.journal = journal;
    }

    pub fn journal(&mut self) -> &Journal {
        self.refresh();
        &self.journal
    }

    pub fn raw_event(&mut self, id: u64) -> Option<Value> {
        self.refresh();
        self.state
            .events()
            .iter()
            .find(|event| event.id == id)
            .and_then(|event| serde_json::to_value(event).ok())
    }

    pub fn raw_task(&mut self, id: u64) -> Option<Value> {
        self.refresh();
        task_record(&self.state, id)
    }
}

impl Journal {
    pub fn read(dir: &Path) -> Self {
        let path = dir.join("log.jsonl");
        match inspect_journal(path) {
            Ok(scan) => Self {
                events: scan.events,
                error: scan.error,
                torn: scan.torn,
            },
            Err(error) => Self {
                events: Vec::new(),
                error: Some(error.to_string()),
                torn: false,
            },
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Health {
    pub model: bool,
    pub model_name: String,
    pub state: bool,
    pub tools: bool,
    pub journal: &'static str,
    pub journal_error: Option<String>,
    pub torn: bool,
    pub turn_in_flight: bool,
    pub capability_errors: Vec<String>,
}

impl serde::Serialize for Health {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Health", 9)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("modelName", &self.model_name)?;
        state.serialize_field("state", &self.state)?;
        state.serialize_field("tools", &self.tools)?;
        state.serialize_field("journal", &self.journal)?;
        state.serialize_field("journalError", &self.journal_error)?;
        state.serialize_field("torn", &self.torn)?;
        state.serialize_field("turnInFlight", &self.turn_in_flight)?;
        state.serialize_field("capabilityErrors", &self.capability_errors)?;
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
        "digest_at": task.reported_at,
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
    cache: &mut Cache,
    after: u64,
    health: &Health,
    busy: bool,
    encounter: Option<Encounter>,
) -> Value {
    cache.refresh();
    render_state(&cache.state, &cache.events, after, health, busy, encounter)
}

fn render_state(
    state: &State,
    event_views: &[Value],
    after: u64,
    health: &Health,
    busy: bool,
    encounter: Option<Encounter>,
) -> Value {
    let events: Vec<Value> = state
        .events()
        .iter()
        .zip(event_views)
        .filter(|(event, _)| event.id > after)
        .map(|(_, view)| view.clone())
        .collect();
    let tasks: Vec<Value> = state
        .tasks()
        .iter()
        .map(|task| task_json(state, task))
        .collect();
    let mut view = json!({
        "events": events,
        "tasks": &tasks,
        "lastEvent": state.last_id(),
        "health": health,
        "presence": presence_voice(state),
    });
    let loops = open_loops(state, &tasks, busy);
    let loops = match encounter {
        None => loops,
        Some(bookmark) => loops
            .into_iter()
            .filter(|item| in_encounter(item, state, bookmark))
            .collect(),
    };
    view["loops"] = Value::Array(loops);
    view["spoken"] = encounter.map_or_else(
        || Value::Array(Vec::new()),
        |bookmark| spoken(state, bookmark),
    );
    view
}

/// The exchange an encounter shows: what was said since its bookmark, without
/// eja's inner voice. An eja message belongs to the conversation only when the
/// user was present in its turn.
fn spoken(state: &State, bookmark: Encounter) -> Value {
    Value::Array(
        state
            .events()
            .iter()
            .filter(|event| event.id > bookmark.anchor)
            .filter(|event| matches!(event.kind, Kind::User | Kind::Eja))
            .filter(|event| event.kind == Kind::User || state.present(event.turn))
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
            event.kind == Kind::Eja
                && event.turn == latest.turn
                && (!event.text.trim().is_empty()
                    || event
                        .data
                        .as_ref()
                        .and_then(|data| data.get("choices"))
                        .and_then(Value::as_array)
                        .is_some_and(|choices| !choices.is_empty()))
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

#[cfg(test)]
pub fn journal_task(journal: &Journal, id: u64) -> Option<Value> {
    let state = State::fold(journal.events.clone());
    task_record(&state, id)
}

fn task_record(state: &State, id: u64) -> Option<Value> {
    let task = state.task(id)?;
    let events: Vec<Value> = state
        .events()
        .iter()
        .filter(|event| task.event_ids.contains(&event.id))
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    Some(json!({ "task": task_json(state, task), "events": events }))
}

#[cfg(test)]
mod tests {
    use super::{journal_task, render_state, state_json, Cache, Encounter, Health, Journal};
    use eja::{Event, Kind};
    use serde_json::{json, Value};
    use std::io::Write as _;

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
            tools: false,
            journal: "ok",
            journal_error: None,
            torn: false,
            turn_in_flight: false,
            capability_errors: vec!["connector unavailable".into()],
        }
    }

    fn view(journal: &Journal, health: &Health, busy: bool, encounter: Option<Encounter>) -> Value {
        let state = eja::State::fold(journal.events.clone());
        let views: Vec<Value> = journal
            .events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        render_state(&state, &views, 0, health, busy, encounter)
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
                json!({"kind": "witness", "target": "environment:out", "arguments": {}, "result": {"ok": true}}),
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
        let state = view(&journal, &health(), false, None);

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
        assert_eq!(
            state["health"]["capabilityErrors"],
            json!(["connector unavailable"])
        );

        let raw = journal_task(&journal, 2).expect("task");
        assert_eq!(raw["task"]["title"], "build");
        assert_eq!(raw["events"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn kernel_and_http_agree_on_the_committed_prefix_for_the_same_bytes() {
        for (name, tail, expected, torn) in [
            ("complete", "\n", 2, false),
            ("missing-newline", "", 1, true),
            ("truncated", "", 1, true),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "eja-committed-prefix-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            eja::Store::new(&dir).expect("initialize state");
            let first = event(1, 1, "eja", Kind::User, "one", json!({}));
            let bytes = if name == "truncated" {
                format!("{}\n{{\"id\":2,", serde_json::to_string(&first).unwrap())
            } else {
                let second = event(2, 2, "eja", Kind::User, "two", json!({}));
                format!(
                    "{}\n{}{tail}",
                    serde_json::to_string(&first).unwrap(),
                    serde_json::to_string(&second).unwrap()
                )
            };
            std::fs::write(dir.join("log.jsonl"), bytes).unwrap();

            let http = Journal::read(&dir);
            let mut cache = Cache::new(dir.clone());
            assert_eq!(http.events.len(), expected, "{name}");
            assert_eq!(http.torn, torn, "{name}");
            assert!(cache.raw_event(1).is_some(), "{name}");
            assert_eq!(cache.raw_event(2).is_some(), expected == 2, "{name}");

            let kernel = eja::Store::new(&dir).expect("kernel store");
            assert_eq!(
                http.events.iter().map(|event| event.id).collect::<Vec<_>>(),
                kernel
                    .events()
                    .expect("kernel events")
                    .iter()
                    .map(|event| event.id)
                    .collect::<Vec<_>>(),
                "{name}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
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
        let state = view(&journal, &health(), false, None);
        assert_eq!(state["presence"], Value::Null);
        let journal = Journal {
            events: vec![
                event(1, 1, "eja", Kind::Returned, "", json!({})),
                event(2, 1, "eja", Kind::Eja, "welcome back", json!({})),
            ],
            error: None,
            torn: false,
        };
        let state = view(&journal, &health(), false, None);
        assert_eq!(state["presence"]["text"], "welcome back");
    }

    #[test]
    fn a_silent_return_question_still_reaches_presence() {
        let journal = Journal {
            events: vec![
                event(1, 1, "eja", Kind::Returned, "", json!({})),
                event(
                    2,
                    1,
                    "eja",
                    Kind::Eja,
                    "",
                    json!({
                        "choices": [
                            {"label": "the earlier bug", "value": {"task": 3}},
                            {"label": "the open review", "value": {"task": 7}}
                        ]
                    }),
                ),
            ],
            error: None,
            torn: false,
        };
        let state = view(&journal, &health(), false, None);
        assert_eq!(state["presence"]["text"], "");
        assert_eq!(state["presence"]["data"]["choices"][1]["value"]["task"], 7);
    }

    #[test]
    fn choice_questions_and_answers_reach_the_view_unchanged() {
        let journal = Journal {
            events: vec![
                event(1, 1, "eja", Kind::User, "open the bug", json!({})),
                event(
                    2,
                    1,
                    "eja",
                    Kind::Eja,
                    "which one?",
                    json!({
                        "choices": [
                            {"label": "the earlier bug", "value": {"task": 3}},
                            {"label": "the open review", "value": {"task": 7}}
                        ]
                    }),
                ),
                event(
                    3,
                    2,
                    "eja",
                    Kind::User,
                    "the open review",
                    json!({"choice": {"question": 2, "option": 1, "value": {"task": 7}}}),
                ),
            ],
            error: None,
            torn: false,
        };
        let state = view(
            &journal,
            &health(),
            false,
            Some(Encounter {
                anchor: 0,
                task: None,
                proposal: None,
            }),
        );
        let spoken = state["spoken"].as_array().unwrap();
        assert_eq!(spoken[1]["data"]["choices"][1]["label"], "the open review");
        assert_eq!(spoken[2]["text"], "the open review");
        assert_eq!(spoken[2]["data"]["choice"]["value"]["task"], 7);
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
        let state = view(&journal, &health(), false, None);
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
        let state = view(&running, &health(), false, None);
        let loops = state["loops"].as_array().unwrap();
        assert_eq!(loops[0]["kind"], "interrupted");
        let state = view(&running, &health(), true, None);
        assert!(state["loops"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] != "interrupted"));
        // The encounter bookmark keeps its own decision and newer events.
        let state = view(
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

    #[test]
    fn the_cache_refreshes_for_appends_and_returns_only_the_requested_tail() {
        let root = std::env::temp_dir().join(format!(
            "eja-host-cache-{}-{}",
            std::process::id(),
            super::super::util::now_seconds()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let first = event(1, 1, "eja", Kind::User, "first", json!({}));
        std::fs::write(
            root.join("log.jsonl"),
            format!("{}\n", serde_json::to_string(&first).unwrap()),
        )
        .unwrap();
        let mut cache = Cache::new(root.clone());
        let initial = state_json(&mut cache, 0, &health(), false, None);
        assert_eq!(initial["events"].as_array().unwrap().len(), 1);

        let second = event(2, 1, "eja", Kind::Eja, "second", json!({}));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("log.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&second).unwrap()).unwrap();

        let tail = state_json(&mut cache, 1, &health(), false, None);
        assert_eq!(tail["events"], json!([second]));
        let _ = std::fs::remove_dir_all(root);
    }
}
