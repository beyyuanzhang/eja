#![allow(clippy::significant_drop_tightening)] // a guard held for one test block is contention-free
//! What a turn's steps see: interleaved reads, worker reports, refusals, a stall.

mod support;

use eja::adapters::{Ports, RecordReader};
use eja::{
    Action, Assignment, Brain, Decision, EjaContext, Kind, ProposalKind, Selector, Store,
    TaskStatus, TurnOutcome, Worker,
};
use std::sync::Mutex;
use support::{aja, decision, task, Done, Script, Seen, State};

/// `support::Script` plus the rendered projection, which only this file asserts on.
struct Rendered {
    script: Script,
    views: Mutex<Vec<String>>,
}

impl Rendered {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            script: Script::new(decisions),
            views: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Vec<Seen>> {
        self.script.seen()
    }
}

impl Brain for Rendered {
    fn decide(&self, context: &EjaContext<'_>, control: &eja::Control) -> Decision {
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(eja::adapters::render_context(context));
        self.script.decide(context, control)
    }
}

#[derive(Default)]
struct CaptureWorker(Mutex<Vec<Assignment>>);

impl Worker for CaptureWorker {
    fn execute(&self, assignment: &Assignment, _: &eja::Control) -> String {
        self.0.lock().unwrap().push(assignment.clone());
        format!(
            "1 steps, 1 tool calls, project: unchanged, ended: message\n{}",
            "worker claim"
        )
    }
}

struct Reader;

impl RecordReader for Reader {
    fn slice(&self, path: &str, _: &Selector, _: &eja::Control) -> eja::Result<serde_json::Value> {
        Ok(serde_json::json!({"path":path,"text":format!("content of {path}"),"truncated":false}))
    }
}

/// One turn whose host reads records, as every scenario here does.
fn reading<'a>(
    store: &Store,
    input: &'a str,
    brain: &dyn Brain,
    worker: &dyn Worker,
) -> TurnOutcome<'a> {
    eja::turn(
        store,
        input,
        brain,
        worker,
        &Ports {
            aja: Some(&Reader),
            ..Default::default()
        },
    )
    .expect("turn")
}

/// A look at one address that selects the whole view.
fn look(target: &str) -> Action {
    Action::Look {
        target: target.into(),
        arguments: serde_json::Value::Null,
        selector: Selector::Whole,
    }
}

/// Close a task, accepted, with the note this scenario reads.
fn close(task_id: u64) -> Action {
    Action::CloseTask {
        task_id,
        accepted: true,
        note: "read the output".into(),
    }
}

/// A decision that answers with this line and does these actions.
fn answer(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.into(),
        actions,
        refused: Vec::new(),
    }
}

#[test]
fn interleaved_reads_preserve_focus_evidence_and_mixed_state_writes() {
    let state = State::new("steps-interleaved");
    let store = state.store();
    let fact = store
        .append_simple(Kind::User, "historical fact only in recall", 1, 1)
        .unwrap();
    reading(
        &store,
        "create task",
        &Script::new(vec![
            decision(vec![task("task")]),
            answer("final answer", vec![]),
        ]),
        &Done::default(),
    );
    let brain = Rendered::new(vec![
        decision(vec![
            look(&format!("log:event:{}", fact.id)),
            Action::RecordDecision {
                text: "written once".into(),
            },
        ]),
        decision(vec![
            Action::Focus { task_id: 1 },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "pending once".into(),
            },
        ]),
        decision(vec![look("record:1/first.md")]),
        decision(vec![look("record:1/second.md")]),
        answer("final answer", vec![]),
    ]);
    let out = reading(&store, "inspect", &brain, &Done::default());
    assert_eq!(out.decisions, 5);
    assert_eq!(out.reply, "final answer");
    assert_ne!(out.end, eja::TurnEnd::Stalled);
    assert_eq!(out.context().focus.unwrap(), 1);
    let context = out.context();
    let log_look = context
        .evidence
        .iter()
        .find(|look| look.target.starts_with("log:"))
        .expect("recall");
    assert_eq!(log_look.result["events"][0]["id"], fact.id);
    assert_eq!(log_look.result["events"][0]["kind"], "user");
    let aja_looks = context
        .evidence
        .iter()
        .filter(|look| look.target.starts_with("record:"))
        .count();
    assert_eq!(aja_looks, 2);
    let seen = brain.seen();
    let views = brain.views.lock().unwrap();
    assert!(seen[1]
        .evidence
        .iter()
        .any(|look| look["result"]["events"][0]["id"] == fact.id));
    assert_eq!(seen[2].proposals, 1);
    for (context, rendered) in seen[2..].iter().zip(&views[2..]) {
        assert_eq!(context.focus.unwrap(), 1);
        assert!(context.evidence.iter().any(|look| look["target"]
            .as_str()
            .is_some_and(|target| target.starts_with("log:"))));
        assert!(rendered.contains(&fact.text));
    }
    assert!(seen[4]
        .evidence
        .iter()
        .any(|look| look["result"]["text"].as_str() == Some("content of 1/second.md")));
    let events = state.events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == Kind::RecordDecision && e.text == "written once")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == Kind::Proposal && e.text == "pending once")
            .count(),
        1
    );
}

#[test]
fn worker_reports_use_the_same_loop_and_allow_sequential_assignments() {
    let state = State::new("steps-worker");
    let store = state.store();
    let fact = store
        .append_simple(Kind::User, "old evidence", 1, 1)
        .unwrap();
    let worker = CaptureWorker::default();
    let brain = Script::new(vec![
        decision(vec![aja("task")]),
        decision(vec![look(&format!("log:event:{}", fact.id))]),
        decision(vec![look("record:1/1.md")]),
        decision(vec![close(1), aja("task")]),
        decision(vec![look("record:1/2.md")]),
        answer("both accepted", vec![close(2)]),
    ]);
    let out = reading(&store, "work", &brain, &worker);
    assert_eq!(out.reply, "both accepted");
    assert_eq!(out.decisions, 6);
    assert_eq!(out.dispatched, vec![1, 2]);
    assert_eq!(out.dispatched.len(), 2);
    assert!(out
        .context()
        .tasks
        .iter()
        .all(|task| task.status == TaskStatus::Done));
    let seen = brain.seen();
    let focused = seen[1]
        .tasks
        .iter()
        .find(|task| Some(task.id) == seen[1].focus)
        .expect("focus is one of the tasks");
    assert_eq!(focused.status, TaskStatus::Waiting);
    assert_eq!(seen[5].focus.unwrap(), 2);
    assert!(seen[5].evidence.iter().any(|look| look["target"]
        .as_str()
        .is_some_and(|target| target.starts_with("log:"))));
    assert!(seen[5]
        .evidence
        .iter()
        .any(|look| look["result"]["path"] == "1/2.md"));
    assert_eq!(worker.0.lock().unwrap().len(), 2);
}

#[test]
fn observations_do_not_allow_confirmation_in_the_proposals_own_turn() {
    let state = State::new("steps-proposal");
    let store = state.store();
    let brain = Script::new(vec![
        decision(vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "not yet confirmed".into(),
            },
            look("record:1/background.md"),
        ]),
        decision(vec![Action::Confirm { proposal_id: 2 }]),
        decision(vec![]),
    ]);
    let out = reading(&store, "propose", &brain, &Done::default());
    // The refused confirmation is told once, so EJA gets one more decision.
    assert_eq!(out.decisions, 3);
    assert_eq!(
        out.context().refused.len(),
        1,
        "{:?}",
        out.context().refused
    );
    assert!(
        out.context().refused[0].contains("created this turn"),
        "{:?}",
        out.context().refused
    );
    assert_eq!(out.context().proposals.len(), 1);
    assert!(out.context().commitments.is_empty());
    assert!(!state
        .events()
        .iter()
        .any(|e| e.kind == Kind::ProposalConfirmed));
}

#[test]
fn repeated_observations_end_the_turn_when_the_view_stops_changing() {
    let state = State::new("steps-stall");
    let store = state.store();
    let out = support::turn_at(
        &store,
        "inspect",
        (0..10)
            .map(|_| decision(vec![look("record:1/missing.md")]))
            .collect(),
    );
    assert_eq!(out.end, eja::TurnEnd::Stalled);
    // The same read twice: the second look carries no new information.
    assert_eq!(out.decisions, 2);
    assert!(out.reply.starts_with("[turn Stalled]"));
    assert_eq!(out.context().evidence.len(), 1);
    assert!(out
        .context()
        .evidence
        .iter()
        .all(|look| look.error.is_some()));
    let events = state.events();
    assert_eq!(
        events.last().unwrap().data.as_ref().unwrap()["end"],
        "stalled"
    );
    assert!(events.last().unwrap().text.starts_with("[turn Stalled]"));
}

#[test]
fn a_stalled_turn_keeps_its_report_and_never_redispatches_after_restart() {
    let state = State::new("steps-restart");
    let store = state.store();
    let mut steps: Vec<Decision> = vec![
        decision(vec![look("record:1/background.md")]),
        decision(vec![look("record:1/second.md")]),
        decision(vec![aja("task")]),
    ];
    // Keep observing the same artifact until the view stops changing.
    steps.extend((0..8).map(|_| decision(vec![look("record:1/same.md")])));
    let worker = CaptureWorker::default();
    let out = reading(&store, "work", &Script::new(steps), &worker);
    assert_eq!(out.end, eja::TurnEnd::Stalled);
    assert!(out.reply.starts_with("[turn Stalled]"));
    assert_eq!(out.context().tasks[0].status, TaskStatus::Waiting);
    assert_eq!(out.dispatched.len(), 1);
    let restarted = state.reopen();
    let brain = Script::new(vec![
        decision(vec![look("record:1/1.md")]),
        answer("accepted after restart", vec![close(1)]),
    ]);
    let resumed = reading(&restarted, "continue", &brain, &worker);
    assert_ne!(resumed.end, eja::TurnEnd::Stalled);
    assert_eq!(resumed.context().tasks[0].status, TaskStatus::Done);
    assert_eq!(brain.seen()[0].focus.unwrap(), 1);
    assert_eq!(worker.0.lock().unwrap().len(), 1);
    assert!(resumed.dispatched.is_empty());
}
