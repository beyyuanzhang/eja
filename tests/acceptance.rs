mod support;

use eja::adapters::{Capabilities, Ports};
use eja::{Action, Decision, Host, Kind, Store, TaskStatus, TurnOptions, TurnOutcome};
use serde_json::{json, Value};
use support::{decision, Done, Script, State};

/// Acceptance evidence is a host act, declared as one capability call.
struct Checks;
impl Capabilities for Checks {
    fn witness(&self, request: &Value, _: &eja::Control) -> eja::Result<Value> {
        Ok(json!({"tool": request["tool"], "passed": true}))
    }
}

fn run_steps() -> Vec<Decision> {
    vec![
        decision(vec![
            Action::OpenTask {
                title: "ship".into(),
                request: "ship it".into(),
                acceptance: "it ships".into(),
                workspace: None,
                evidence: Some(json!({"tool": "checks", "arguments": {}})),
                run: false,
            },
            Action::Dispatch { task_id: 1 },
        ]),
        decision(vec![]),
    ]
}

/// One scripted turn against the host this file chooses.
fn turn(
    store: &Store,
    input: &'static str,
    steps: Vec<Decision>,
    host: &dyn Host,
) -> TurnOutcome<'static> {
    let brain = Script::new(steps);
    eja::turn(store, input, &brain, &Done::default(), host).expect("turn")
}

/// The one thing the kit does not cover: a turn the host authorizes in advance.
fn approved(
    store: &Store,
    input: &'static str,
    steps: Vec<Decision>,
    approvals: Vec<u64>,
) -> TurnOutcome<'static> {
    let brain = Script::new(steps);
    eja::turn_at_with_options(
        store,
        input,
        &brain,
        &Done::default(),
        &Ports::default(),
        eja::now_utc(),
        &TurnOptions {
            approvals,
            ..Default::default()
        },
    )
    .expect("turn")
}

#[test]
fn declared_evidence_is_a_host_observation_and_the_judgment_can_use_it() {
    let fixture = State::new("accept");
    let store = fixture.store();
    let checks = Checks;
    let ports = Ports {
        capabilities: Some(&checks),
        ..Default::default()
    };
    let out = turn(&store, "do it", run_steps(), &ports);
    let task = &out.context().tasks[0];
    assert_eq!(task.status, TaskStatus::Waiting);
    assert_eq!(task.evidence().expect("request")["tool"], json!("checks"));
    assert_eq!(
        task.run
            .as_ref()
            .and_then(|run| run.evidence_result.as_ref())
            .expect("evidence")["passed"],
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
        task.evidence_id(),
        Some(observed.id),
        "the task points at the observation"
    );

    let closed = turn(
        &store,
        "accept",
        vec![decision(vec![Action::CloseTask {
            task_id: 1,
            accepted: true,
            note: "evidence passed".into(),
        }])],
        &ports,
    );
    assert_eq!(closed.context().tasks[0].status, TaskStatus::Done);
}

#[test]
fn without_host_evidence_the_task_cannot_be_accepted_but_can_be_rejected() {
    let fixture = State::new("accept-no-evidence");
    let store = fixture.store();
    let out = turn(&store, "do it", run_steps(), &Ports::default());
    assert!(
        out.context().tasks[0].evidence().is_some(),
        "the request is a fact"
    );
    assert!(out.context().tasks[0]
        .run
        .as_ref()
        .and_then(|run| run.evidence_result.as_ref())
        .is_none());

    let closed = turn(
        &store,
        "accept",
        vec![decision(vec![Action::CloseTask {
            task_id: 1,
            accepted: true,
            note: "looks fine".into(),
        }])],
        &Ports::default(),
    );
    assert_eq!(closed.context().tasks[0].status, TaskStatus::Waiting);
    assert!(
        closed
            .context()
            .refused
            .iter()
            .any(|note| note.contains("needs the evidence")),
        "{:?}",
        closed.context().refused
    );

    // Rejecting needs no evidence: a witness that never answers cannot strand
    // the task in the index.
    let rejected = turn(
        &store,
        "reject",
        vec![decision(vec![Action::CloseTask {
            task_id: 1,
            accepted: false,
            note: "evidence unavailable".into(),
        }])],
        &Ports::default(),
    );
    assert_eq!(rejected.context().tasks[0].status, TaskStatus::Failed);
    assert!(rejected.context().tasks[0]
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
    let fixture = State::new("accept-broken");
    let store = fixture.store();
    let broken = Broken;
    let ports = Ports {
        capabilities: Some(&broken),
        ..Default::default()
    };
    turn(&store, "do it", run_steps(), &ports);
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
    let fixture = State::new("accept-approval");
    let store = fixture.store();
    let first = turn(
        &store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind: eja::ProposalKind::Commitment,
            target_id: None,
            text: "answer first".into(),
        }])],
        &Ports::default(),
    );
    let id = first.context().proposals[0].id;
    assert!(
        first
            .context()
            .proposals
            .iter()
            .all(|proposal| !proposal.approved),
        "nothing is approved until the host records it"
    );
    approved(
        &store,
        "approve but do not confirm",
        vec![decision(vec![])],
        vec![id],
    );
    let stale = turn(
        &store,
        "confirm late",
        vec![decision(vec![Action::Confirm { proposal_id: id }])],
        &Ports::default(),
    );
    assert!(stale.context().commitments.is_empty());
    assert!(
        stale
            .context()
            .refused
            .iter()
            .any(|note| note.contains("no approval")),
        "{:?}",
        stale.context().refused
    );
}
