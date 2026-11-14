use eja::adapters::{NodeReader, Ports};
use eja::{
    turn, Action, Assignment, Brain, Decision, EjaContext, Kind, LookSource, ProposalKind,
    Selector, Store, TaskStatus, Worker,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

static SERIAL: AtomicU64 = AtomicU64::new(0);

fn store() -> Store {
    let root = std::env::temp_dir().join(format!(
        "eja-steps-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    Store::new(root).unwrap()
}

struct Script {
    decisions: Mutex<VecDeque<Decision>>,
    seen: Mutex<Vec<EjaContext>>,
}

impl Script {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into()),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Brain for Script {
    fn decide(&self, context: &EjaContext, _: &eja::Control) -> Decision {
        self.seen.lock().unwrap().push(context.clone());
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted decision")
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

impl NodeReader for Reader {
    fn slice(&self, path: &str, _: &Selector, _: &eja::Control) -> eja::Result<serde_json::Value> {
        Ok(serde_json::json!({"path":path,"text":format!("content of {path}"),"truncated":false}))
    }
}

fn step(actions: Vec<Action>) -> Decision {
    Decision {
        reply: "intermediate reply".into(),
        actions,
        refused: Vec::new(),
    }
}

fn done() -> Decision {
    Decision {
        reply: "final answer".into(),
        actions: vec![],
        refused: Vec::new(),
    }
}

fn open(run: bool) -> Action {
    Action::OpenTask {
        title: "task".into(),
        request: "work".into(),
        acceptance: "read the output".into(),
        run,
    }
}

fn close(task_id: u64) -> Action {
    Action::CloseTask {
        task_id,
        accepted: true,
        note: "read the output".into(),
    }
}

#[test]
fn interleaved_reads_preserve_focus_evidence_and_mixed_state_writes() {
    let store = store();
    let fact = store
        .append_simple(Kind::User, "historical fact only in recall", 1, 1)
        .unwrap();
    let worker = CaptureWorker::default();
    turn(
        &store,
        "create task",
        &Script::new(vec![step(vec![open(false)]), done()]),
        &worker,
        &Ports {
            node: Some(&Reader),
            ..Default::default()
        },
    )
    .unwrap();
    let brain = Script::new(vec![
        step(vec![
            Action::Look {
                source: LookSource::Log,
                target: format!("event:{}", fact.id),
                arguments: serde_json::Value::Null,
                selector: Selector::Whole,
            },
            Action::RecordDecision {
                text: "written once".into(),
            },
        ]),
        step(vec![
            Action::Focus { task_id: 1 },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "pending once".into(),
            },
        ]),
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "first.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "second.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        done(),
    ]);
    let out = turn(
        &store,
        "inspect",
        &brain,
        &worker,
        &Ports {
            node: Some(&Reader),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.decisions, 5);
    assert_eq!(out.reply, "final answer");
    assert!(out.end != eja::TurnEnd::Stalled);
    assert_eq!(out.context.focus.as_ref().unwrap().id, 1);
    let log_look = out
        .evidence
        .iter()
        .find(|look| look.source == LookSource::Log)
        .expect("recall");
    assert_eq!(log_look.result["events"][0]["id"], fact.id);
    assert_eq!(log_look.result["events"][0]["kind"], "user");
    let node_looks = out
        .evidence
        .iter()
        .filter(|look| look.source == LookSource::Node)
        .count();
    assert_eq!(node_looks, 2);
    let seen = brain.seen.lock().unwrap();
    assert!(seen[1]
        .evidence
        .iter()
        .any(|look| look.result["events"][0]["id"] == fact.id));
    assert_eq!(seen[2].proposals.len(), 1);
    for context in &seen[2..] {
        assert_eq!(context.focus.as_ref().unwrap().id, 1);
        assert!(context
            .evidence
            .iter()
            .any(|look| look.source == LookSource::Log));
        assert!(eja::adapters::render_context(context).contains(&fact.text));
    }
    assert!(seen[4]
        .evidence
        .iter()
        .any(|look| look.result["text"].as_str() == Some("content of second.md")));
    let events = store.events().unwrap();
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
    let store = store();
    let fact = store
        .append_simple(Kind::User, "old evidence", 1, 1)
        .unwrap();
    let worker = CaptureWorker::default();
    let brain = Script::new(vec![
        step(vec![open(true)]),
        step(vec![Action::Look {
            source: LookSource::Log,
            target: format!("event:{}", fact.id),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "1.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        step(vec![close(1), open(true)]),
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "2.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        Decision {
            reply: "both accepted".into(),
            actions: vec![close(2)],
            refused: Vec::new(),
        },
    ]);
    let out = turn(
        &store,
        "work",
        &brain,
        &worker,
        &Ports {
            node: Some(&Reader),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.reply, "both accepted");
    assert_eq!(out.decisions, 6);
    assert_eq!(out.dispatched, vec![1, 2]);
    assert_eq!(out.dispatched.len(), 2);
    assert!(out.tasks.iter().all(|task| task.status == TaskStatus::Done));
    let seen = brain.seen.lock().unwrap();
    assert_eq!(seen[1].focus.as_ref().unwrap().status, TaskStatus::Waiting);
    assert_eq!(seen[5].focus.as_ref().unwrap().id, 2);
    assert!(seen[5]
        .evidence
        .iter()
        .any(|look| look.source == LookSource::Log));
    assert!(seen[5]
        .evidence
        .iter()
        .any(|look| look.result["path"] == "2.md"));
    assert_eq!(worker.0.lock().unwrap().len(), 2);
}

#[test]
fn observations_do_not_allow_confirmation_in_the_proposals_own_turn() {
    let store = store();
    let brain = Script::new(vec![
        step(vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "not yet confirmed".into(),
            },
            Action::Look {
                source: LookSource::Node,
                target: "background.md".into(),
                arguments: serde_json::Value::Null,
                selector: Selector::Whole,
            },
        ]),
        step(vec![Action::Confirm { proposal_id: 2 }]),
        step(vec![]),
    ]);
    let out = turn(
        &store,
        "propose",
        &brain,
        &CaptureWorker::default(),
        &Ports {
            node: Some(&Reader),
            ..Default::default()
        },
    )
    .unwrap();
    // The refused confirmation is told once, so EJA gets one more decision.
    assert_eq!(out.decisions, 3);
    assert_eq!(out.refused.len(), 1, "{:?}", out.refused);
    assert!(out.refused[0].contains("host-recorded user approval"));
    assert_eq!(out.context.proposals.len(), 1);
    assert!(out.context.commitments.is_empty());
    assert!(!store
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == Kind::ProposalConfirmed));
}

#[test]
fn repeated_observations_end_the_turn_when_the_view_stops_changing() {
    let store = store();
    let brain = Script::new(
        (0..10)
            .map(|_| {
                step(vec![Action::Look {
                    source: LookSource::Node,
                    target: "missing.md".into(),
                    arguments: serde_json::Value::Null,
                    selector: Selector::Whole,
                }])
            })
            .collect(),
    );
    let worker = CaptureWorker::default();
    let out = turn(&store, "inspect", &brain, &worker, &Ports::default()).unwrap();
    assert!(out.end == eja::TurnEnd::Stalled);
    // The same read twice: the second look carries no new information.
    assert_eq!(out.decisions, 2);
    assert!(out.reply.starts_with("[turn Stalled]"));
    assert_eq!(out.evidence.len(), 1);
    assert!(out.context.evidence.iter().all(|look| look.error.is_some()));
    let events = store.events().unwrap();
    assert_eq!(
        events.last().unwrap().data.as_ref().unwrap()["end"],
        "stalled"
    );
    assert!(events.last().unwrap().text.starts_with("[turn Stalled]"));
}

#[test]
fn a_stalled_turn_keeps_its_report_and_never_redispatches_after_restart() {
    let store = store();
    let mut steps: Vec<Decision> = vec![
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "background.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "second.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        step(vec![open(true)]),
    ];
    // Keep observing the same artifact until the view stops changing.
    steps.extend((0..8).map(|_| {
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "same.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }])
    }));
    let worker = CaptureWorker::default();
    let out = turn(
        &store,
        "work",
        &Script::new(steps),
        &worker,
        &Ports {
            node: Some(&Reader),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(out.end == eja::TurnEnd::Stalled);
    assert!(out.reply.starts_with("[turn Stalled]"));
    assert_eq!(out.tasks[0].status, TaskStatus::Waiting);
    assert_eq!(out.dispatched.len(), 1);
    let restarted = Store::new(store.log_path.parent().unwrap()).unwrap();
    let brain = Script::new(vec![
        step(vec![Action::Look {
            source: LookSource::Node,
            target: "1.md".into(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        Decision {
            reply: "accepted after restart".into(),
            actions: vec![close(1)],
            refused: Vec::new(),
        },
    ]);
    let resumed = turn(
        &restarted,
        "continue",
        &brain,
        &worker,
        &Ports {
            node: Some(&Reader),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(resumed.end != eja::TurnEnd::Stalled);
    assert_eq!(resumed.tasks[0].status, TaskStatus::Done);
    assert_eq!(brain.seen.lock().unwrap()[0].focus.as_ref().unwrap().id, 1);
    assert_eq!(worker.0.lock().unwrap().len(), 1);
    assert!(resumed.dispatched.is_empty());
}
