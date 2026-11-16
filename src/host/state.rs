use eja::{Event, Kind};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

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

pub struct Folded {
    events: Vec<Value>,
    messages: Vec<Value>,
    tasks: Vec<Value>,
    relations: Vec<Value>,
    proposals: Vec<Value>,
    identity: Map<String, Value>,
    refused: Vec<Value>,
    last_turn: u64,
}

impl Folded {
    pub fn task(&self, id: u64) -> Option<&Value> {
        self.tasks
            .iter()
            .find(|task| task.get("id").and_then(Value::as_u64) == Some(id))
    }

    pub fn state(&self, now: i64, state_dir: &Path, project_dir: &Path, health: &Health) -> Value {
        let count = |status: &str| {
            self.tasks
                .iter()
                .filter(|task| task.get("status").and_then(Value::as_str) == Some(status))
                .count()
        };
        let goal = self
            .identity
            .get("goals")
            .and_then(Value::as_array)
            .and_then(|goals| goals.first())
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "now": now,
            "stateDir": state_dir.display().to_string(),
            "projectDir": project_dir.display().to_string(),
            "events": self.events,
            "messages": self.messages,
            "tasks": self.tasks,
            "relations": self.relations,
            "proposals": self.proposals,
            "identity": Value::Object(self.identity.clone()),
            "currentGoal": goal,
            "refused": self.refused,
            "counts": {
                "tasks": self.tasks.len(),
                "open": count("open"),
                "running": count("running"),
                "waiting": count("waiting"),
                "done": count("done"),
                "failed": count("failed"),
                "messages": self.messages.len(),
            },
            "lastTurn": self.last_turn,
            "lastEvent": self
                .events
                .last()
                .and_then(|event| event.get("id"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            "health": health,
        })
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Health {
    pub binary: bool,
    pub binary_path: String,
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
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Health", 10)?;
        state.serialize_field("binary", &self.binary)?;
        state.serialize_field("binaryPath", &self.binary_path)?;
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

pub fn journal_events(journal: &Journal) -> Vec<Value> {
    journal
        .events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect()
}

pub fn fold(journal: &Journal) -> Folded {
    let events = journal_events(journal);
    let mut tasks = BTreeMap::<u64, Value>::new();
    let mut task_events = BTreeMap::<u64, Vec<u64>>::new();
    let mut relations = Vec::new();
    let mut proposals = BTreeMap::<u64, Value>::new();
    let mut closed = BTreeSet::new();
    let mut goals = Vec::new();
    let mut commitments = Vec::new();
    let mut corrections = Vec::new();
    let mut decisions = Vec::new();
    let mut relationships = Vec::new();
    let mut messages = Vec::new();
    let mut refused = Vec::new();
    let mut last_turn = 0;

    for event in &journal.events {
        last_turn = last_turn.max(event.turn);
        if let Some(id) = event.closes {
            closed.insert(id);
        }
        if matches!(event.kind, Kind::User | Kind::Eja) {
            messages.push(json!({
                "id": event.id,
                "role": event.kind.as_str(),
                "text": event.text,
                "at": event.at,
                "turn": event.turn,
                "data": event.data,
            }));
        }
        let task_id = scope_task_id(&event.scope);
        if let Some(task_id) = task_id {
            task_events.entry(task_id).or_default().push(event.id);
        }
        let data = event.data.as_ref().and_then(Value::as_object);
        match event.kind {
            Kind::TaskOpened => {
                let Some(id) = task_id else {
                    continue;
                };
                let kind = data
                    .and_then(|data| data.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("aja");
                let title = data
                    .and_then(|data| data.get("title"))
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map_or_else(
                        || {
                            if event.text.is_empty() {
                                format!("task {id}")
                            } else {
                                event.text.clone()
                            }
                        },
                        ToOwned::to_owned,
                    );
                tasks.insert(
                    id,
                    json!({
                        "id": id,
                        "kind": if kind == "group" { "group" } else { "aja" },
                        "title": title,
                        "request": string_field(data, "request"),
                        "origin": string_field(data, "origin"),
                        "acceptance": string_field(data, "acceptance"),
                        "status": "open",
                        "created_at": event.at,
                        "assigned_at": null,
                        "reported_at": null,
                        "closed_at": null,
                        "close": null,
                        "assignment": null,
                        "run": {
                            "digest": null,
                            "evidence": data.and_then(|data| data.get("evidence")).cloned().unwrap_or(Value::Null),
                            "evidence_id": null,
                            "evidence_result": null,
                            "witness_at": null,
                            "reconcile": null,
                        },
                        "event_ids": [event.id],
                        "children": [],
                    }),
                );
            }
            Kind::TaskAssigned => {
                if let Some(task) = task_id.and_then(|id| tasks.get_mut(&id)) {
                    task["status"] = json!("running");
                    task["assigned_at"] = json!(event.at);
                    task["assignment"] = event.data.clone().unwrap_or(Value::Null);
                }
            }
            Kind::WorkerReport => {
                if let Some(task) = task_id.and_then(|id| tasks.get_mut(&id)) {
                    task["status"] = json!("waiting");
                    task["reported_at"] = json!(event.at);
                    task["run"]["digest"] = json!(event.text);
                }
            }
            Kind::Observed => {
                let observed = data
                    .and_then(|data| data.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if observed == "witness" {
                    if let Some(task) = task_id.and_then(|id| tasks.get_mut(&id)) {
                        task["run"]["evidence_id"] = json!(event.id);
                        task["run"]["evidence_result"] = data
                            .and_then(|data| data.get("result"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        task["run"]["witness_at"] = json!(event.at);
                    }
                } else if observed == "reconcile" {
                    let target = data
                        .and_then(|data| data.get("target"))
                        .and_then(Value::as_str)
                        .and_then(scope_task_id)
                        .or(task_id);
                    if let Some(task) = target.and_then(|id| tasks.get_mut(&id)) {
                        task["run"]["reconcile"] = data
                            .and_then(|data| data.get("result"))
                            .cloned()
                            .unwrap_or(Value::Null);
                    }
                }
            }
            Kind::TaskClosed => {
                if let Some(task) = task_id.and_then(|id| tasks.get_mut(&id)) {
                    let accepted = data
                        .and_then(|data| data.get("accepted"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    task["close"] = event.data.clone().unwrap_or(Value::Null);
                    task["closed_at"] = json!(event.at);
                    task["status"] = json!(if accepted { "done" } else { "failed" });
                }
            }
            Kind::TaskRelation => {
                let from = data
                    .and_then(|data| data.get("from"))
                    .and_then(Value::as_u64);
                let to = data.and_then(|data| data.get("to")).and_then(Value::as_u64);
                let kind = data
                    .and_then(|data| data.get("kind"))
                    .and_then(Value::as_str);
                if let (Some(from), Some(to), Some(kind)) = (from, to, kind) {
                    relations.push(json!({ "from": from, "to": to, "kind": kind }));
                }
            }
            Kind::Proposal => {
                let kind = data
                    .and_then(|data| data.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("change");
                let target_id = data
                    .and_then(|data| data.get("target_id"))
                    .and_then(Value::as_u64);
                proposals.insert(
                    event.id,
                    json!({
                        "id": event.id,
                        "kind": kind,
                        "target_id": target_id,
                        "text": event.text,
                        "at": event.at,
                        "turn": event.turn,
                    }),
                );
            }
            Kind::ProposalConfirmed | Kind::ProposalRejected => {
                if let Some(id) = data
                    .and_then(|data| data.get("proposal_id"))
                    .and_then(Value::as_u64)
                {
                    proposals.remove(&id);
                }
            }
            Kind::Goal | Kind::Commitment | Kind::Correction => {
                let proposal_id = data
                    .and_then(|data| data.get("proposal_id"))
                    .and_then(Value::as_u64);
                if let Some(id) = proposal_id {
                    proposals.remove(&id);
                }
                let entry = json!({
                    "id": event.id,
                    "text": event.text,
                    "at": event.at,
                    "closes": event.closes,
                    "proposal_id": proposal_id,
                });
                match event.kind {
                    Kind::Goal => goals.push((event.id, entry)),
                    Kind::Commitment => commitments.push((event.id, entry)),
                    _ => corrections.push((event.id, entry)),
                }
            }
            Kind::RecordDecision => decisions.push((
                event.id,
                json!({ "id": event.id, "text": event.text, "at": event.at }),
            )),
            Kind::Relationship => relationships.push((
                event.id,
                json!({ "id": event.id, "text": event.text, "at": event.at }),
            )),
            Kind::Refused => refused.push(json!({
                "id": event.id,
                "text": event.text,
                "at": event.at,
                "scope": event.scope,
            })),
            _ => {}
        }
    }

    for task in tasks.values_mut() {
        if let Some(ids) =
            task_events.get(&task.get("id").and_then(Value::as_u64).unwrap_or_default())
        {
            task["event_ids"] = json!(ids);
        }
    }
    let task_ids: BTreeSet<u64> = tasks.keys().copied().collect();
    for relation in &relations {
        if relation.get("kind").and_then(Value::as_str) != Some("part_of") {
            continue;
        }
        let child = relation.get("from").and_then(Value::as_u64);
        let parent = relation.get("to").and_then(Value::as_u64);
        if let (Some(child), Some(parent)) = (child, parent) {
            if child != parent && task_ids.contains(&child) && task_ids.contains(&parent) {
                if let Some(children) = tasks
                    .get_mut(&parent)
                    .and_then(|task| task.get_mut("children"))
                    .and_then(Value::as_array_mut)
                {
                    children.push(json!(child));
                }
            }
        }
    }

    sort_desc(&mut goals, &closed, true);
    sort_desc(&mut commitments, &closed, true);
    sort_desc(&mut corrections, &closed, false);
    sort_desc(&mut decisions, &closed, false);
    sort_desc(&mut relationships, &closed, false);
    let identity = Map::from_iter([
        ("goals".to_owned(), values(goals)),
        ("commitments".to_owned(), values(commitments)),
        ("corrections".to_owned(), values(corrections)),
        ("decisions".to_owned(), values(decisions)),
        ("relationships".to_owned(), values(relationships)),
    ]);
    Folded {
        events,
        messages,
        tasks: tasks.into_values().collect(),
        relations,
        proposals: proposals.into_values().collect(),
        identity,
        refused,
        last_turn,
    }
}

fn string_field(data: Option<&Map<String, Value>>, name: &str) -> String {
    data.and_then(|data| data.get(name))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn scope_task_id(scope: &str) -> Option<u64> {
    scope.strip_prefix("task:")?.parse().ok()
}

fn sort_desc(entries: &mut Vec<(u64, Value)>, closed: &BTreeSet<u64>, filter_closed: bool) {
    entries.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    if filter_closed {
        entries.retain(|(id, _)| !closed.contains(id));
    }
}

fn values(entries: Vec<(u64, Value)>) -> Value {
    Value::Array(entries.into_iter().map(|(_, value)| value).collect())
}

pub fn journal_event(journal: &Journal, id: u64) -> Option<Value> {
    journal
        .events
        .iter()
        .find(|event| event.id == id)
        .and_then(|event| serde_json::to_value(event).ok())
}

pub fn journal_task(journal: &Journal, id: u64) -> Option<Value> {
    let folded = fold(journal);
    let task = folded.task(id)?.clone();
    let ids: BTreeSet<u64> = task
        .get("event_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect();
    let events: Vec<Value> = journal
        .events
        .iter()
        .filter(|event| ids.contains(&event.id))
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    Some(json!({ "task": task, "events": events }))
}

#[cfg(test)]
mod tests {
    use super::{fold, journal_task, Health, Journal};
    use eja::{Event, Kind};
    use serde_json::{json, Value};
    use std::path::Path;

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
            binary: true,
            binary_path: "eja".to_owned(),
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
    fn the_projection_keeps_tasks_messages_and_edges() {
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
                json!({"task_id": 2}),
            ),
            event(6, 1, "task:2", Kind::WorkerReport, "built", json!({})),
            event(
                7,
                1,
                "task:2",
                Kind::Observed,
                "",
                json!({"kind": "witness", "result": {"ok": true}}),
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
        let folded = fold(&journal);
        let state = folded.state(
            1_700_000_100,
            Path::new("/state"),
            Path::new("/project"),
            &health(),
        );

        assert_eq!(state["messages"].as_array().unwrap().len(), 2);
        assert_eq!(state["events"].as_array().unwrap().len(), 9);
        assert_eq!(state["counts"]["done"], 1);
        assert_eq!(state["counts"]["tasks"], 2);
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
        assert_eq!(state["relations"][0]["kind"], "part_of");

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
