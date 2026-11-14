use eja::adapters::{render_context, Ports};
use eja::{
    active_identity, open_commitments, tasks, turn_at, Action, Assignment, Brain, Decision,
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
    fn decide(&self, _context: &EjaContext, _: &eja::Control) -> Decision {
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
    fn execute(&self, _assignment: &Assignment, _: &eja::Control) -> String {
        self.queue
            .lock()
            .expect("worker")
            .pop_front()
            .unwrap_or_else(|| {
                format!(
                    "0 steps, 0 tool calls, project: unchanged, ended: error\n{}",
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

fn record(summary: &str) -> String {
    format!(
        "1 steps, 1 tool calls, project: unchanged, ended: message\n{}",
        summary
    )
}

fn temp_store() -> std::path::PathBuf {
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eja-core-{}-{}", std::process::id(), serial));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn now() -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(2026, 11, 9, 12, 0, 0)
        .single()
        .expect("time")
}

fn propose_and_confirm(
    store: &Store,
    kind: ProposalKind,
    text: &str,
    target_id: Option<u64>,
    at: chrono::DateTime<chrono::Utc>,
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
fn closed_loop_dispatches_reports_and_accepts() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain::new(vec![
        decision(
            "working",
            vec![Action::OpenTask {
                title: "release checklist".to_string(),
                request: "write a release checklist".to_string(),
                acceptance: "contains build, smoke test, upload".to_string(),
                run: true,
            }],
        ),
        decision(
            "done",
            vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "checked".to_string(),
            }],
        ),
    ]);
    let worker = QueueWorker::new(vec![record("checklist written")]);

    let outcome = turn_at(
        &store,
        "write a release checklist",
        &brain,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("turn");

    assert_eq!(outcome.reply, "done");
    assert_eq!(outcome.tasks.len(), 1);
    assert_eq!(outcome.tasks[0].status, TaskStatus::Done);
    assert_eq!(outcome.tasks[0].origin, "write a release checklist");
    assert_eq!(outcome.dispatched, vec![1]);
    assert_eq!(outcome.dispatched.len(), 1);

    let events = store.events().expect("events");
    let kinds: Vec<Kind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            Kind::User,
            Kind::TaskOpened,
            Kind::TaskAssigned,
            Kind::WorkerReport,
            Kind::TaskClosed,
            Kind::Eja
        ]
    );

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["log.jsonl".to_string(), "log.turn.lock".to_string()]
    );
    assert_eq!(
        std::fs::metadata(dir.join("log.turn.lock")).unwrap().len(),
        0,
        "the lease stores no second state"
    );
}

#[test]
fn rejection_fails_the_task_and_keeps_the_report() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain::new(vec![
        decision(
            "start",
            vec![Action::OpenTask {
                title: "migration".to_string(),
                request: "write the migration".to_string(),
                acceptance: "contains backup, dual-write, cutover".to_string(),
                run: true,
            }],
        ),
        decision(
            "reject",
            vec![Action::CloseTask {
                task_id: 1,
                accepted: false,
                note: "missing cutover".to_string(),
            }],
        ),
    ]);
    let digest = format!(
        "0 steps, 0 tool calls, project: unchanged, ended: budget\n{}",
        "only backup and dual-write"
    );

    let outcome = turn_at(
        &store,
        "do the migration",
        &brain,
        &QueueWorker::new(vec![digest]),
        &Ports::default(),
        now(),
    )
    .expect("turn");

    assert_eq!(outcome.tasks[0].status, TaskStatus::Failed);
    assert!(!outcome.tasks[0].close.as_ref().expect("close").accepted);
    assert!(outcome.tasks[0]
        .digest
        .as_deref()
        .unwrap_or("")
        .contains("ended: budget"));
}

#[test]
fn impossible_actions_are_refused_visibly_and_change_nothing() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let first = QueueBrain::new(vec![
        decision(
            "start",
            vec![Action::OpenTask {
                title: "one run".to_string(),
                request: "do the work".to_string(),
                acceptance: "a record exists".to_string(),
                run: true,
            }],
        ),
        // The task now has a node record: dispatching it again is refused.
        decision("again", vec![Action::Dispatch { task_id: 1 }]),
    ]);
    let worker = QueueWorker::new(vec![record("first run")]);
    let outcome = turn_at(
        &store,
        "one task",
        &first,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("first turn");
    assert_eq!(outcome.refused.len(), 1, "{:?}", outcome.refused);
    assert!(
        outcome.refused[0].contains("waiting"),
        "{:?}",
        outcome.refused
    );
    assert!(outcome.dispatched.len() == 1);

    // Unknown ids are refused too, and nothing is appended for them.
    let second = QueueBrain::new(vec![decision(
        "close early",
        vec![
            Action::CloseTask {
                task_id: 9,
                accepted: true,
                note: "nothing".to_string(),
            },
            Action::Dispatch { task_id: 9 },
        ],
    )]);
    let outcome = turn_at(
        &store,
        "close early",
        &second,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("second turn");
    assert_eq!(outcome.refused.len(), 2, "{:?}", outcome.refused);
    assert!(outcome
        .refused
        .iter()
        .any(|note| note.contains("close refused")));
    assert!(outcome
        .refused
        .iter()
        .any(|note| note.contains("no task 9")));
    let events = store.events().expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == Kind::TaskAssigned)
            .count(),
        1
    );
    assert!(events.iter().all(|event| event.kind != Kind::TaskClosed));
    // A refused action becomes a host note in the log and in the next projection:
    // EJA can read its own dead ends instead of repeating them.
    assert!(events.iter().any(|event| event.kind == Kind::Refused));
    let rendered = render_context(&outcome.context);
    assert!(rendered.contains("REFUSED"), "{rendered}");
    assert!(rendered.contains("no task 9"), "{rendered}");
}

#[test]
fn identity_is_reconstructed_and_worker_reports_are_not_identity() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "answer with the conclusion first",
        None,
        now(),
    );

    let events = store.events().expect("events");
    assert_eq!(open_commitments(&events).len(), 1);
    assert!(!active_identity(&events)
        .iter()
        .any(|event| event.kind == Kind::WorkerReport));
}

#[test]
fn task_index_and_read_task_are_structured() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let open = QueueBrain::new(vec![
        decision(
            "open A",
            vec![Action::OpenTask {
                title: "release".to_string(),
                request: "write release steps".to_string(),
                acceptance: "contains build".to_string(),
                run: false,
            }],
        ),
        decision("opened A", vec![]),
        decision(
            "open B",
            vec![Action::OpenTask {
                title: "migration".to_string(),
                request: "write migration steps".to_string(),
                acceptance: "contains backup".to_string(),
                run: false,
            }],
        ),
    ]);
    let worker = QueueWorker::new(vec![]);
    turn_at(&store, "open A", &open, &worker, &Ports::default(), now()).expect("A");
    turn_at(&store, "open B", &open, &worker, &Ports::default(), now()).expect("B");

    let read = QueueBrain::new(vec![
        decision("reading", vec![Action::Focus { task_id: 1 }]),
        decision("found", vec![]),
    ]);
    let outcome = turn_at(
        &store,
        "look at the first task",
        &read,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("read");

    assert_eq!(outcome.context.focus.as_ref().expect("focus").id, 1);
    assert_eq!(outcome.context.tasks.len(), 2);
}

#[test]
fn explicit_dispatch_runs_an_open_task() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain::new(vec![
        decision(
            "open",
            vec![Action::OpenTask {
                title: "organize".to_string(),
                request: "organize the files".to_string(),
                acceptance: "both summaries exist".to_string(),
                run: false,
            }],
        ),
        decision("opened", vec![]),
        decision("start", vec![Action::Dispatch { task_id: 1 }]),
        decision(
            "done",
            vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "ok".to_string(),
            }],
        ),
    ]);
    let worker = QueueWorker::new(vec![record("organized")]);
    turn_at(
        &store,
        "open a task",
        &brain,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("open");
    let outcome =
        turn_at(&store, "run it", &brain, &worker, &Ports::default(), now()).expect("dispatch");
    assert_eq!(outcome.dispatched, vec![1]);
    assert_eq!(outcome.tasks[0].status, TaskStatus::Done);
}

#[test]
fn cannot_close_without_a_worker_report() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain::new(vec![decision(
        "close",
        vec![
            Action::OpenTask {
                title: "no record".to_string(),
                request: "do something".to_string(),
                acceptance: "has a result".to_string(),
                run: false,
            },
            Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "empty".to_string(),
            },
        ],
    )]);
    let outcome = turn_at(
        &store,
        "open and close",
        &brain,
        &QueueWorker::new(vec![]),
        &Ports::default(),
        now(),
    )
    .expect("turn");
    assert_eq!(outcome.tasks[0].status, TaskStatus::Open);
    assert!(outcome.tasks[0].close.is_none());
    assert!(!store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::TaskClosed));
}

#[test]
fn worker_failure_stays_waiting_for_eja() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain::new(vec![
        decision(
            "start",
            vec![Action::OpenTask {
                title: "failing".to_string(),
                request: "execute".to_string(),
                acceptance: "complete".to_string(),
                run: true,
            }],
        ),
        decision("worker failed", vec![]),
    ]);
    let digest = format!(
        "0 steps, 0 tool calls, project: unchanged, ended: error\n{}",
        "failed"
    );
    let outcome = turn_at(
        &store,
        "run",
        &brain,
        &QueueWorker::new(vec![digest]),
        &Ports::default(),
        now(),
    )
    .expect("turn");
    assert_eq!(outcome.tasks[0].status, TaskStatus::Waiting);
    assert!(outcome.tasks[0]
        .digest
        .as_deref()
        .unwrap_or("")
        .contains("ended: error"));
}

#[test]
fn correction_targets_an_event_id_not_a_phrase() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "arbitrary text",
        None,
        now(),
    );
    let commitment = open_commitments(&store.events().expect("events"))[0].clone();

    let wrong = QueueBrain::new(vec![decision(
        "wrong target",
        vec![Action::Propose {
            kind: ProposalKind::Correction,
            target_id: Some(999),
            text: "whatever".to_string(),
        }],
    )]);
    turn_at(
        &store,
        "correct",
        &wrong,
        &QueueWorker::new(vec![]),
        &Ports::default(),
        now(),
    )
    .expect("wrong");
    assert_eq!(open_commitments(&store.events().expect("events")).len(), 1);

    propose_and_confirm(
        &store,
        ProposalKind::Correction,
        "whatever",
        Some(commitment.id),
        now(),
    );
    let events = store.events().expect("events");
    assert!(open_commitments(&events).is_empty());
    assert!(!tasks(&events)
        .iter()
        .any(|task| task.status == TaskStatus::Running));
}

#[test]
fn multiple_dispatches_use_one_execution_owner_and_preserve_order() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct OverlapWorker {
        running: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }
    impl Worker for OverlapWorker {
        fn execute(&self, assignment: &Assignment, _: &eja::Control) -> String {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(150));
            self.running.fetch_sub(1, Ordering::SeqCst);
            format!(
                "1 steps, 1 tool calls, project: unchanged, ended: message\nnode {} done",
                assignment.task_id
            )
        }
    }

    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let opener = QueueBrain::new(vec![decision(
        "two tasks",
        vec![
            Action::OpenTask {
                title: "a".to_string(),
                request: "do a".to_string(),
                acceptance: "a holds".to_string(),
                run: false,
            },
            Action::OpenTask {
                title: "b".to_string(),
                request: "do b".to_string(),
                acceptance: "b holds".to_string(),
                run: false,
            },
        ],
    )]);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let worker = OverlapWorker {
        running: running.clone(),
        peak: peak.clone(),
    };
    turn_at(
        &store,
        "two tasks",
        &opener,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("open");
    let brain = QueueBrain::new(vec![
        decision(
            "dispatch both",
            vec![
                Action::Dispatch { task_id: 1 },
                Action::Dispatch { task_id: 2 },
            ],
        ),
        decision("done", vec![]),
    ]);
    let outcome = turn_at(
        &store,
        "dispatch",
        &brain,
        &worker,
        &Ports::default(),
        now(),
    )
    .expect("turn");
    assert_eq!(outcome.dispatched, vec![1, 2]);
    assert_eq!(peak.load(Ordering::SeqCst), 1, "one execution owner");
    assert_eq!(
        outcome.tasks.iter().filter(|t| t.digest.is_some()).count(),
        2
    );
}
