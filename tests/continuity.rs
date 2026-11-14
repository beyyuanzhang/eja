use chrono::{TimeZone, Utc};
use eja::adapters::Ports;
use eja::{
    current_goal, open_commitments, project, turn_at, Action, Assignment, Brain, Decision,
    EjaContext, Kind, ProposalKind, Store, TaskStatus, Worker,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct QueueBrain {
    queue: Mutex<VecDeque<Decision>>,
}

impl QueueBrain {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            queue: Mutex::new(decisions.into()),
        }
    }
}

impl Brain for QueueBrain {
    fn decide(&self, _context: &EjaContext) -> Decision {
        self.queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: String::new(),
                actions: Vec::new(),
                refused: Vec::new(),
            })
    }
}

struct QueueWorker {
    queue: Mutex<VecDeque<String>>,
}

impl QueueWorker {
    fn new(reports: Vec<String>) -> Self {
        Self {
            queue: Mutex::new(reports.into()),
        }
    }
}

impl Worker for QueueWorker {
    fn execute(&self, _assignment: &Assignment) -> String {
        self.queue
            .lock()
            .expect("worker")
            .pop_front()
            .unwrap_or_else(|| {
                format!(
                    "0 steps, 0 tool calls, 0 artifacts, ended: error\n{}",
                    "no scripted record"
                )
            })
    }
}

fn decision(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.to_string(),
        actions,
        refused: Vec::new(),
    }
}

fn record() -> String {
    format!(
        "1 steps, 1 tool calls, 1 artifacts, ended: message\n{}",
        "done"
    )
}

fn temp_store() -> std::path::PathBuf {
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("eja-continuity-{}-{}", std::process::id(), serial));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn day(year: i32, month: u32, day: u32, hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
        .single()
        .expect("valid UTC time")
}

fn propose_and_confirm(
    store: &Store,
    kind: ProposalKind,
    text: &str,
    target_id: Option<u64>,
    at: chrono::DateTime<Utc>,
) {
    let propose = QueueBrain::new(vec![decision(
        "proposed",
        vec![Action::Propose {
            kind,
            target_id,
            text: text.to_string(),
        }],
    )]);
    let outcome = turn_at(
        store,
        "propose",
        &propose,
        &QueueWorker::new(vec![]),
        &Ports::default(),
        at,
    )
    .expect("propose");
    let proposal_id = outcome
        .context
        .proposals
        .last()
        .expect("pending proposal")
        .id;
    let confirm = QueueBrain::new(vec![decision(
        "confirmed",
        vec![Action::Confirm { proposal_id }],
    )]);
    eja::turn_at_with_options(
        store,
        "confirm",
        &confirm,
        &QueueWorker::new(vec![]),
        &Ports::default(),
        at,
        &eja::TurnOptions {
            approvals: vec![proposal_id],
            ..Default::default()
        },
    )
    .expect("confirm");
}

#[test]
fn user_input_does_not_become_a_goal() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let outcome = turn_at(
        &store,
        "what is the weather?",
        &QueueBrain::new(vec![decision("a reply", vec![])]),
        &QueueWorker::new(vec![]),
        &Ports::default(),
        day(2026, 11, 9, 9),
    )
    .expect("turn");
    assert_eq!(outcome.context.input, "what is the weather?");
    assert!(outcome.context.goal.is_none());
    let restarted = Store::new(&dir).expect("restart");
    assert!(current_goal(&restarted.events().expect("events")).is_empty());
    assert!(eja::adapters::render_context(&outcome.context).contains("CURRENT GOAL:\n  (none)"));
}

#[test]
fn unrelated_input_preserves_a_confirmed_goal() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Goal,
        "ship the release",
        None,
        day(2026, 11, 9, 9),
    );
    let outcome = turn_at(
        &store,
        "what is the weather?",
        &QueueBrain::new(vec![decision("a reply", vec![])]),
        &QueueWorker::new(vec![]),
        &Ports::default(),
        day(2026, 11, 9, 10),
    )
    .expect("turn");
    let goal = outcome.context.goal.as_ref().expect("goal");
    assert_eq!(goal.text, "ship the release");
    assert!(eja::adapters::render_context(&outcome.context)
        .contains(&format!("#{} ship the release", goal.id)));
    assert_eq!(outcome.context.input, "what is the weather?");
}

#[test]
fn restart_and_model_switch_preserve_identity() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "answer with the conclusion first",
        None,
        day(2026, 11, 9, 9),
    );

    let store = Store::new(&dir).expect("restart");
    let other_model = QueueBrain::new(vec![decision("continue", vec![])]);
    turn_at(
        &store,
        "what did I commit to?",
        &other_model,
        &QueueWorker::new(vec![]),
        &Ports::default(),
        day(2026, 11, 10, 9),
    )
    .expect("continue");

    let context =
        project(&store, "what did I commit to?", None, day(2026, 11, 10, 10)).expect("context");
    assert_eq!(context.commitments.len(), 1);
    assert_eq!(
        context.commitments[0].text,
        "answer with the conclusion first"
    );
}

#[test]
fn correction_by_id_survives_restart() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "answer with the conclusion first",
        None,
        day(2026, 11, 9, 9),
    );
    let commitment = open_commitments(&store.events().expect("events"))[0].clone();

    propose_and_confirm(
        &store,
        ProposalKind::Correction,
        "drop that commitment",
        Some(commitment.id),
        day(2026, 11, 9, 10),
    );

    let store = Store::new(&dir).expect("restart");
    let events = store.events().expect("events");
    assert!(open_commitments(&events).is_empty());
    assert!(events
        .iter()
        .any(|event| event.kind == Kind::Correction && event.closes == Some(commitment.id)));
}

#[test]
fn correction_can_close_a_goal_without_language_matching() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Goal,
        "old objective",
        None,
        day(2026, 11, 9, 9),
    );
    let goal_event = store
        .events()
        .expect("events")
        .into_iter()
        .find(|event| event.kind == Kind::Goal)
        .expect("goal event");

    propose_and_confirm(
        &store,
        ProposalKind::Correction,
        "withdraw the goal",
        Some(goal_event.id),
        day(2026, 11, 9, 10),
    );
    let events = store.events().expect("events");
    assert!(current_goal(&events).is_empty());
}

#[test]
fn replacements_preserve_their_kind_and_revision_chain_across_restart() {
    for kind in [ProposalKind::Goal, ProposalKind::Commitment] {
        let dir = temp_store();
        let store = Store::new(&dir).expect("store");
        let event_kind = match kind {
            ProposalKind::Goal => Kind::Goal,
            ProposalKind::Commitment => Kind::Commitment,
            ProposalKind::Correction => unreachable!(),
        };
        let mut previous = None;
        for text in ["original", "replacement", "latest"] {
            propose_and_confirm(&store, kind, text, previous, day(2026, 11, 9, 9));
            let events = store.events().expect("events");
            let entry = events
                .iter()
                .rev()
                .find(|e| e.kind == event_kind)
                .expect("entry");
            assert_eq!(entry.text, text);
            assert_eq!(entry.closes, previous);
            previous = Some(entry.id);
        }
        let restarted = Store::new(&dir).expect("restart");
        let context = project(&restarted, "unrelated input", None, day(2026, 11, 10, 9)).unwrap();
        if kind == ProposalKind::Goal {
            assert_eq!(context.goal.as_ref().expect("goal").text, "latest");
        } else {
            assert_eq!(context.commitments.len(), 1);
            assert_eq!(context.commitments[0].text, "latest");
        }
        propose_and_confirm(
            &restarted,
            ProposalKind::Correction,
            "withdraw latest",
            previous,
            day(2026, 11, 10, 10),
        );
        let events = Store::new(&dir).unwrap().events().unwrap();
        assert!(current_goal(&events).is_empty());
        assert!(open_commitments(&events).is_empty());
        assert_eq!(events.iter().filter(|e| e.kind == event_kind).count(), 3);
    }
}

#[test]
fn utc_time_is_one_source_for_turn_task_and_worker() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let at = day(2026, 11, 9, 12);
    let brain = QueueBrain::new(vec![
        decision(
            "start",
            vec![Action::OpenTask {
                title: "release".to_string(),
                request: "write checklist".to_string(),
                acceptance: "contains build".to_string(),
                run: true,
            }],
        ),
        decision(
            "done",
            vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "ok".to_string(),
            }],
        ),
    ]);
    let outcome = turn_at(
        &store,
        "make a checklist",
        &brain,
        &QueueWorker::new(vec![record()]),
        &Ports::default(),
        at,
    )
    .expect("turn");

    assert_eq!(outcome.context.now, at.timestamp());
    let task = &outcome.tasks[0];
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.created_at, at.timestamp());
    assert_eq!(task.digest_at, Some(at.timestamp()));
    assert_eq!(task.closed_at, Some(at.timestamp()));

    let events = store.events().expect("events");
    let assignment = events
        .iter()
        .find(|event| event.kind == Kind::TaskAssigned)
        .and_then(|event| event.data.as_ref())
        .and_then(|data| serde_json::from_value::<Assignment>(data.clone()).ok())
        .expect("assignment");
    assert_eq!(assignment.issued_at, at.timestamp());
}

#[test]
fn temporal_gap_is_numeric_and_language_free() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "deliver Wednesday",
        None,
        day(2026, 11, 9, 9),
    );

    let later = day(2026, 11, 12, 18);
    let context = project(&store, "what is due?", None, later).expect("context");
    assert_eq!(context.now, later.timestamp());
    assert_eq!(context.commitments[0].at, day(2026, 11, 9, 9).timestamp());
}
