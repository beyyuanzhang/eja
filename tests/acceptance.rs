use eja::adapters::{Capabilities, Ports};
use eja::{
    turn_at, turn_at_with_options, Action, Assignment, Brain, Decision, EjaContext, Kind, Store,
    TaskStatus, TurnOptions,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;

struct Queue(Mutex<VecDeque<Decision>>);
impl Queue {
    fn new(steps: Vec<Vec<Action>>) -> Self {
        Self(Mutex::new(
            steps
                .into_iter()
                .map(|actions| Decision {
                    reply: "ok".to_string(),
                    actions,
                    refused: Vec::new(),
                })
                .collect(),
        ))
    }
}
impl Brain for Queue {
    fn decide(&self, _: &EjaContext, _: &eja::Control) -> Decision {
        self.0
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

struct NoWorker;
impl eja::Worker for NoWorker {
    fn execute(&self, _: &Assignment, _: &eja::Control) -> String {
        "1 steps, 0 tool calls, ended: message\ndone".to_string()
    }
}

/// Acceptance evidence is a host act, declared as one capability call.
struct Checks;
impl Capabilities for Checks {
    fn witness(&self, request: &Value, _: &eja::Control) -> eja::Result<Value> {
        Ok(json!({"tool": request["tool"], "passed": true}))
    }
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn store() -> Store {
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eja-accept-{}-{}", std::process::id(), serial));
    let _ = std::fs::remove_dir_all(&dir);
    Store::new(&dir).expect("store")
}

fn run_steps() -> Vec<Vec<Action>> {
    vec![
        vec![
            Action::OpenTask {
                title: "ship".into(),
                request: "ship it".into(),
                acceptance: "it ships".into(),
                evidence: Some(json!({"tool": "checks", "arguments": {}})),
                run: false,
            },
            Action::Dispatch { task_id: 1 },
        ],
        vec![],
    ]
}

#[test]
fn declared_evidence_is_a_host_observation_and_the_judgment_can_use_it() {
    let store = store();
    let checks = Checks;
    let ports = Ports {
        capabilities: Some(&checks),
        ..Default::default()
    };
    let out = turn_at(
        &store,
        "do it",
        &Queue::new(run_steps()),
        &NoWorker,
        &ports,
        eja::now_utc(),
    )
    .expect("turn");
    let task = &out.context.tasks[0];
    assert_eq!(task.status, TaskStatus::Waiting);
    assert_eq!(
        task.evidence.as_ref().expect("request")["tool"],
        json!("checks")
    );
    assert_eq!(
        task.evidence_result.as_ref().expect("evidence")["passed"],
        json!(true)
    );

    // The evidence is one observation, scoped to the task it answers.
    let observed = store
        .events()
        .expect("events")
        .into_iter()
        .find(|event| {
            event.kind == Kind::Observed
                && event.scope == "task:1"
                && event
                    .data
                    .as_ref()
                    .is_some_and(|data| data["kind"] == json!("witness"))
        })
        .expect("witness observation");
    assert_eq!(
        task.evidence_id,
        Some(observed.id),
        "the task points at the observation"
    );

    let closed = turn_at(
        &store,
        "accept",
        &Queue::new(vec![vec![Action::CloseTask {
            task_id: 1,
            accepted: true,
            note: "evidence passed".into(),
        }]]),
        &NoWorker,
        &ports,
        eja::now_utc(),
    )
    .expect("close");
    assert_eq!(closed.context.tasks[0].status, TaskStatus::Done);
}

#[test]
fn without_host_evidence_the_task_cannot_be_accepted_but_can_be_rejected() {
    let store = store();
    let out = turn_at(
        &store,
        "do it",
        &Queue::new(run_steps()),
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("turn");
    assert!(
        out.context.tasks[0].evidence.is_some(),
        "the request is a fact"
    );
    assert!(out.context.tasks[0].evidence_result.is_none());

    let closed = turn_at(
        &store,
        "accept",
        &Queue::new(vec![vec![Action::CloseTask {
            task_id: 1,
            accepted: true,
            note: "looks fine".into(),
        }]]),
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("close");
    assert_eq!(closed.context.tasks[0].status, TaskStatus::Waiting);
    assert!(
        closed
            .context
            .refused
            .iter()
            .any(|note| note.contains("needs the evidence")),
        "{:?}",
        closed.context.refused
    );

    // Rejecting needs no evidence: a witness that never answers cannot strand
    // the task in the index.
    let rejected = turn_at(
        &store,
        "reject",
        &Queue::new(vec![vec![Action::CloseTask {
            task_id: 1,
            accepted: false,
            note: "evidence unavailable".into(),
        }]]),
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("reject");
    assert_eq!(rejected.context.tasks[0].status, TaskStatus::Failed);
    assert!(rejected.context.tasks[0]
        .close
        .as_ref()
        .is_some_and(|close| !close.accepted));
}

#[test]
fn a_host_that_cannot_answer_leaves_the_reason_in_the_log() {
    struct Broken;
    impl Capabilities for Broken {
        fn witness(&self, _: &Value, _: &eja::Control) -> eja::Result<Value> {
            Err("the checker is offline".into())
        }
    }
    let store = store();
    let broken = Broken;
    let ports = Ports {
        capabilities: Some(&broken),
        ..Default::default()
    };
    turn_at(
        &store,
        "do it",
        &Queue::new(run_steps()),
        &NoWorker,
        &ports,
        eja::now_utc(),
    )
    .expect("turn");
    let observation = store
        .events()
        .expect("events")
        .into_iter()
        .find(|event| event.kind == Kind::Observed && event.scope == "task:1")
        .expect("observation");
    let data = observation.data.expect("data");
    assert_eq!(data["error"], json!("the checker is offline"));
    assert!(data["result"].is_null());
}

#[test]
fn an_approval_authorizes_only_the_turn_it_was_recorded_in() {
    let store = store();
    let first = turn_at(
        &store,
        "propose",
        &Queue::new(vec![vec![Action::Propose {
            kind: eja::ProposalKind::Commitment,
            target_id: None,
            text: "answer first".into(),
        }]]),
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("propose");
    let id = first.context.proposals[0].id;
    assert!(
        first
            .context
            .proposals
            .iter()
            .all(|proposal| !proposal.approved),
        "nothing is approved until the host records it"
    );
    turn_at_with_options(
        &store,
        "approve but do not confirm",
        &Queue::new(vec![vec![]]),
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
        &TurnOptions {
            approvals: vec![id],
            ..Default::default()
        },
    )
    .expect("approve");
    let stale = turn_at(
        &store,
        "confirm late",
        &Queue::new(vec![vec![Action::Confirm { proposal_id: id }]]),
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("confirm");
    assert!(stale.context.commitments.is_empty());
    assert!(
        stale
            .context
            .refused
            .iter()
            .any(|note| note.contains("no approval")),
        "{:?}",
        stale.context.refused
    );
}
