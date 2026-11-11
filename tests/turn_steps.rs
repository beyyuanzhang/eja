use eja::{
    turn, Action, ArtifactContent, ArtifactReader, Assignment, Brain, Decision, EjaContext, Event,
    Kind, NoopArtifactReader, NoopRetriever, ProposalKind, Report, ReportStatus, Result, Retriever,
    Store, TaskStatus, Worker, MAX_TURN_STEPS,
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
    fn decide(&self, context: &EjaContext) -> Decision {
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
    fn execute(&self, assignment: &Assignment) -> Report {
        self.0.lock().unwrap().push(assignment.clone());
        Report {
            status: ReportStatus::Done,
            summary: "worker claim".into(),
            artifacts: vec![format!("{}.md", assignment.task_id)],
            evidence: vec![],
            open_questions: vec![],
            tools: Vec::new(),
        }
    }
}

struct FixedRecall(u64);

impl Retriever for FixedRecall {
    fn retrieve(&self, _: &str, _: &[Event]) -> Result<Vec<u64>> {
        Ok(vec![self.0, self.0, u64::MAX])
    }
}

struct Reader;

impl ArtifactReader for Reader {
    fn read(&self, path: &str) -> ArtifactContent {
        ArtifactContent {
            path: path.into(),
            text: format!("content of {path}"),
            truncated: false,
            error: None,
        }
    }
}

fn step(actions: Vec<Action>) -> Decision {
    Decision {
        reply: "intermediate reply".into(),
        actions,
    }
}

fn done() -> Decision {
    Decision {
        reply: "final answer".into(),
        actions: vec![],
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
        &Script::new(vec![step(vec![open(false)])]),
        &worker,
        &NoopRetriever,
        &Reader,
    )
    .unwrap();
    let brain = Script::new(vec![
        step(vec![
            Action::Recall {
                query: "old fact".into(),
            },
            Action::RecordDecision {
                text: "written once".into(),
            },
        ]),
        step(vec![
            Action::ReadTask { task_id: 1 },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "pending once".into(),
            },
        ]),
        step(vec![Action::ReadArtifact {
            path: "first.md".into(),
        }]),
        step(vec![Action::Recall {
            query: "old fact again".into(),
        }]),
        step(vec![Action::ReadArtifact {
            path: "second.md".into(),
        }]),
        done(),
    ]);
    let out = turn(
        &store,
        "inspect",
        &brain,
        &worker,
        &FixedRecall(fact.id),
        &Reader,
    )
    .unwrap();
    assert_eq!(out.decisions, 6);
    assert_eq!(out.reply, "final answer");
    assert!(!out.step_limit_reached);
    assert_eq!(out.reads, vec![1]);
    assert_eq!(out.context.focus.as_ref().unwrap().id, 1);
    assert_eq!(out.recalled.len(), 1);
    assert_eq!(out.recalled[0].id, fact.id);
    assert_eq!(out.artifacts.len(), 2);
    let seen = brain.seen.lock().unwrap();
    assert!(seen[1].recall.iter().any(|e| e.id == fact.id));
    assert_eq!(seen[2].proposals.len(), 1);
    for context in &seen[2..] {
        assert_eq!(context.focus.as_ref().unwrap().id, 1);
        assert_eq!(context.recall.len(), 1);
        assert!(eja::adapters::render_context(context).contains(&fact.text));
    }
    assert!(seen[5]
        .artifacts
        .iter()
        .any(|a| a.text == "content of second.md"));
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
        step(vec![Action::Recall {
            query: "old evidence".into(),
        }]),
        step(vec![Action::ReadTask { task_id: 1 }]),
        step(vec![Action::ReadArtifact {
            path: "1.md".into(),
        }]),
        step(vec![close(1), open(true)]),
        step(vec![Action::ReadArtifact {
            path: "2.md".into(),
        }]),
        Decision {
            reply: "both accepted".into(),
            actions: vec![close(2)],
        },
    ]);
    let out = turn(
        &store,
        "work",
        &brain,
        &worker,
        &FixedRecall(fact.id),
        &Reader,
    )
    .unwrap();
    assert_eq!(out.reply, "both accepted");
    assert_eq!(out.decisions, 7);
    assert_eq!(out.dispatched, vec![1, 2]);
    assert_eq!(out.reports.len(), 2);
    assert!(out.tasks.iter().all(|task| task.status == TaskStatus::Done));
    let seen = brain.seen.lock().unwrap();
    assert_eq!(seen[1].focus.as_ref().unwrap().status, TaskStatus::Waiting);
    assert_eq!(seen[5].focus.as_ref().unwrap().id, 2);
    assert_eq!(seen[5].recall[0].id, fact.id);
    assert!(seen[6].artifacts.iter().any(|a| a.path == "2.md"));
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
            Action::ReadArtifact {
                path: "background.md".into(),
            },
        ]),
        step(vec![Action::Confirm { proposal_id: 2 }]),
    ]);
    let out = turn(
        &store,
        "propose",
        &brain,
        &CaptureWorker::default(),
        &NoopRetriever,
        &Reader,
    )
    .unwrap();
    assert_eq!(out.decisions, 2);
    assert_eq!(out.context.proposals.len(), 1);
    assert!(out.context.commitments.is_empty());
    assert!(!store
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == Kind::ProposalConfirmed));
}

#[test]
fn repeated_observations_stop_at_the_limit_without_a_false_final_reply() {
    let store = store();
    let brain = Script::new(
        (0..MAX_TURN_STEPS)
            .map(|_| {
                step(vec![Action::ReadArtifact {
                    path: "missing.md".into(),
                }])
            })
            .collect(),
    );
    let worker = CaptureWorker::default();
    let out = turn(
        &store,
        "inspect",
        &brain,
        &worker,
        &NoopRetriever,
        &NoopArtifactReader,
    )
    .unwrap();
    assert!(out.step_limit_reached);
    assert_eq!(out.decisions, MAX_TURN_STEPS);
    assert!(out.reply.is_empty());
    assert_eq!(out.artifacts.len(), MAX_TURN_STEPS);
    assert!(out.context.artifacts.iter().all(|a| a.error.is_some()));
    let events = store.events().unwrap();
    assert_eq!(
        events.last().unwrap().data.as_ref().unwrap()["step_limit_reached"],
        true
    );
    assert!(events.last().unwrap().text.is_empty());
}

#[test]
fn report_at_the_limit_survives_restart_and_is_not_dispatched_again() {
    let store = store();
    let mut steps: Vec<Decision> = (1..MAX_TURN_STEPS)
        .map(|_| {
            step(vec![Action::ReadArtifact {
                path: "background.md".into(),
            }])
        })
        .collect();
    steps.push(step(vec![open(true)]));
    let worker = CaptureWorker::default();
    let out = turn(
        &store,
        "work",
        &Script::new(steps),
        &worker,
        &NoopRetriever,
        &Reader,
    )
    .unwrap();
    assert!(out.step_limit_reached);
    assert!(out.reply.is_empty());
    assert_eq!(out.tasks[0].status, TaskStatus::Waiting);
    assert_eq!(out.reports.len(), 1);
    let restarted = Store::new(store.log_path.parent().unwrap()).unwrap();
    let brain = Script::new(vec![
        step(vec![Action::ReadArtifact {
            path: "1.md".into(),
        }]),
        Decision {
            reply: "accepted after restart".into(),
            actions: vec![close(1)],
        },
    ]);
    let resumed = turn(
        &restarted,
        "continue",
        &brain,
        &worker,
        &NoopRetriever,
        &Reader,
    )
    .unwrap();
    assert!(!resumed.step_limit_reached);
    assert_eq!(resumed.tasks[0].status, TaskStatus::Done);
    assert_eq!(brain.seen.lock().unwrap()[0].focus.as_ref().unwrap().id, 1);
    assert_eq!(worker.0.lock().unwrap().len(), 1);
    assert!(resumed.dispatched.is_empty());
}
