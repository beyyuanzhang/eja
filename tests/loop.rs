mod support;
use eja::adapters::{render_context, Ports};
use eja::{
    now_utc, turn_at_with_options, Action, Assignment, Decision, Kind, ProposalKind, State, Store,
    TaskStatus, Worker,
};
use support::{decision, turn_with, Done, Script, State as Fixture};

/// A worker whose report is the exact account a test states.
struct Report(&'static str);
impl Worker for Report {
    fn execute(&self, _: &Assignment, _: &eja::Control) -> String {
        self.0.to_string()
    }
}

fn propose_and_confirm(store: &Store, kind: ProposalKind, text: &str, target_id: Option<u64>) {
    let proposed = turn_with(
        store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind,
            target_id,
            text: text.to_string(),
        }])],
        &Done::default(),
    );
    let proposal_id = proposed
        .context()
        .proposals
        .last()
        .expect("pending proposal")
        .id;
    turn_at_with_options(
        store,
        "confirm",
        &Script::new(vec![decision(vec![Action::Confirm { proposal_id }])]),
        &Done::default(),
        &Ports::default(),
        now_utc(),
        &eja::TurnOptions {
            approvals: vec![proposal_id],
            ..Default::default()
        },
    )
    .expect("confirm");
}

#[test]
fn closed_loop_dispatches_reports_and_accepts() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let decisions = vec![
        decision(vec![Action::OpenTask {
            title: "release checklist".to_string(),
            request: "write a release checklist".to_string(),
            acceptance: "contains build, smoke test, upload".to_string(),
            workspace: None,
            evidence: None,
            run: true,
        }]),
        Decision {
            reply: "done".to_string(),
            actions: vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "checked".to_string(),
            }],
            refused: Vec::new(),
        },
    ];
    let outcome = turn_with(
        &store,
        "write a release checklist",
        decisions,
        &Done("checklist written"),
    );

    assert_eq!(outcome.reply, "done");
    assert_eq!(outcome.context().tasks.len(), 1);
    assert_eq!(outcome.context().tasks[0].status, TaskStatus::Done);
    assert_eq!(
        outcome.context().tasks[0].origin,
        "write a release checklist"
    );
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

    let mut names: Vec<String> = std::fs::read_dir(fixture.path())
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
        std::fs::metadata(fixture.path().join("log.turn.lock"))
            .unwrap()
            .len(),
        0,
        "the lease stores no second state"
    );
}

#[test]
fn rejection_fails_the_task_and_keeps_the_report() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let decisions = vec![
        decision(vec![Action::OpenTask {
            title: "migration".to_string(),
            request: "write the migration".to_string(),
            acceptance: "contains backup, dual-write, cutover".to_string(),
            workspace: None,
            evidence: None,
            run: true,
        }]),
        decision(vec![Action::CloseTask {
            task_id: 1,
            accepted: false,
            note: "missing cutover".to_string(),
        }]),
    ];

    let outcome = turn_with(
        &store,
        "do the migration",
        decisions,
        &Report(
            "0 steps, 0 tool calls, project: unchanged, ended: budget\nonly backup and dual-write",
        ),
    );

    assert_eq!(outcome.context().tasks[0].status, TaskStatus::Failed);
    assert!(
        !outcome.context().tasks[0]
            .close
            .as_ref()
            .expect("close")
            .accepted
    );
    assert!(outcome.context().tasks[0]
        .digest()
        .unwrap_or("")
        .contains("ended: budget"));
}

#[test]
fn impossible_actions_are_refused_visibly_and_change_nothing() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let worker = Done("first run");
    let outcome = turn_with(
        &store,
        "one task",
        vec![
            decision(vec![Action::OpenTask {
                title: "one run".to_string(),
                request: "do the work".to_string(),
                acceptance: "a record exists".to_string(),
                workspace: None,
                evidence: None,
                run: true,
            }]),
            // The task now has a aja record: dispatching it again is refused.
            decision(vec![Action::Dispatch { task_id: 1 }]),
        ],
        &worker,
    );
    assert_eq!(
        outcome.context().refused.len(),
        1,
        "{:?}",
        outcome.context().refused
    );
    assert!(
        outcome.context().refused[0].contains("waiting"),
        "{:?}",
        outcome.context().refused
    );
    assert_eq!(outcome.dispatched.len(), 1);

    // Unknown ids are refused too, and nothing is appended for them.
    let outcome = turn_with(
        &store,
        "close early",
        vec![decision(vec![
            Action::CloseTask {
                task_id: 9,
                accepted: true,
                note: "nothing".to_string(),
            },
            Action::Dispatch { task_id: 9 },
        ])],
        &worker,
    );
    assert_eq!(
        outcome.context().refused.len(),
        2,
        "{:?}",
        outcome.context().refused
    );
    assert!(outcome
        .context()
        .refused
        .iter()
        .any(|note| note.contains("close refused")));
    assert!(outcome
        .context()
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
    let rendered = render_context(&outcome.context());
    assert!(rendered.contains("REFUSED"), "{rendered}");
    assert!(rendered.contains("no task 9"), "{rendered}");
}

#[test]
fn identity_is_reconstructed_and_worker_reports_are_not_identity() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "answer with the conclusion first",
        None,
    );

    let events = store.events().expect("events");
    assert_eq!(State::fold(events.clone()).commitments().len(), 1);
    assert!(!State::fold(events)
        .identity()
        .iter()
        .any(|event| event.kind == Kind::WorkerReport));
}

#[test]
fn task_index_and_read_task_are_structured() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    turn_with(
        &store,
        "open A",
        vec![
            decision(vec![Action::OpenTask {
                title: "release".to_string(),
                request: "write release steps".to_string(),
                acceptance: "contains build".to_string(),
                workspace: None,
                evidence: None,
                run: false,
            }]),
            decision(vec![]),
        ],
        &Done::default(),
    );
    turn_with(
        &store,
        "open B",
        vec![decision(vec![Action::OpenTask {
            title: "migration".to_string(),
            request: "write migration steps".to_string(),
            acceptance: "contains backup".to_string(),
            workspace: None,
            evidence: None,
            run: false,
        }])],
        &Done::default(),
    );

    let outcome = turn_with(
        &store,
        "look at the first task",
        vec![
            decision(vec![Action::Focus { task_id: 1 }]),
            decision(vec![]),
        ],
        &Done::default(),
    );

    assert_eq!(outcome.context().focus.expect("focus"), 1);
    assert_eq!(outcome.context().tasks.len(), 2);
}

#[test]
fn explicit_dispatch_runs_an_open_task() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let worker = Done("organized");
    turn_with(
        &store,
        "open a task",
        vec![
            decision(vec![Action::OpenTask {
                title: "organize".to_string(),
                request: "organize the files".to_string(),
                acceptance: "both summaries exist".to_string(),
                workspace: None,
                evidence: None,
                run: false,
            }]),
            decision(vec![]),
        ],
        &worker,
    );
    let outcome = turn_with(
        &store,
        "run it",
        vec![
            decision(vec![Action::Dispatch { task_id: 1 }]),
            decision(vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "ok".to_string(),
            }]),
        ],
        &worker,
    );
    assert_eq!(outcome.dispatched, vec![1]);
    assert_eq!(outcome.context().tasks[0].status, TaskStatus::Done);
}

#[test]
fn cannot_close_without_a_worker_report() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let outcome = turn_with(
        &store,
        "open and close",
        vec![decision(vec![
            Action::OpenTask {
                title: "no record".to_string(),
                request: "do something".to_string(),
                acceptance: "has a result".to_string(),
                workspace: None,
                evidence: None,
                run: false,
            },
            Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "empty".to_string(),
            },
        ])],
        &Done::default(),
    );
    assert_eq!(outcome.context().tasks[0].status, TaskStatus::Open);
    assert!(outcome.context().tasks[0].close.is_none());
    assert!(!store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::TaskClosed));
}

#[test]
fn worker_failure_stays_waiting_for_eja() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let outcome = turn_with(
        &store,
        "run",
        vec![
            decision(vec![Action::OpenTask {
                title: "failing".to_string(),
                request: "execute".to_string(),
                acceptance: "complete".to_string(),
                workspace: None,
                evidence: None,
                run: true,
            }]),
            decision(vec![]),
        ],
        &Report("0 steps, 0 tool calls, project: unchanged, ended: error\nfailed"),
    );
    assert_eq!(outcome.context().tasks[0].status, TaskStatus::Waiting);
    assert!(outcome.context().tasks[0]
        .digest()
        .unwrap_or("")
        .contains("ended: error"));
}

#[test]
fn correction_targets_an_event_id_not_a_phrase() {
    let fixture = Fixture::new("loop");
    let store = fixture.store();
    propose_and_confirm(&store, ProposalKind::Commitment, "arbitrary text", None);
    let commitment = State::load(&store).expect("state").commitments()[0].clone();

    turn_with(
        &store,
        "correct",
        vec![decision(vec![Action::Propose {
            kind: ProposalKind::Correction,
            target_id: Some(999),
            text: "whatever".to_string(),
        }])],
        &Done::default(),
    );
    assert_eq!(State::load(&store).expect("state").commitments().len(), 1);

    propose_and_confirm(
        &store,
        ProposalKind::Correction,
        "whatever",
        Some(commitment.id),
    );
    let events = store.events().expect("events");
    assert!(State::fold(events.clone()).commitments().is_empty());
    assert!(!State::fold(events)
        .tasks()
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
                "1 steps, 1 tool calls, project: unchanged, ended: message\naja {} done",
                assignment.task_id
            )
        }
    }

    let fixture = Fixture::new("loop");
    let store = fixture.store();
    let opener = vec![decision(vec![
        Action::OpenTask {
            title: "a".to_string(),
            request: "do a".to_string(),
            acceptance: "a holds".to_string(),
            workspace: None,
            evidence: None,
            run: false,
        },
        Action::OpenTask {
            title: "b".to_string(),
            request: "do b".to_string(),
            acceptance: "b holds".to_string(),
            workspace: None,
            evidence: None,
            run: false,
        },
    ])];
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let worker = OverlapWorker {
        running,
        peak: peak.clone(),
    };
    turn_with(&store, "two tasks", opener, &worker);
    let outcome = turn_with(
        &store,
        "dispatch",
        vec![
            decision(vec![
                Action::Dispatch { task_id: 1 },
                Action::Dispatch { task_id: 2 },
            ]),
            decision(vec![]),
        ],
        &worker,
    );
    assert_eq!(outcome.dispatched, vec![1, 2]);
    assert_eq!(peak.load(Ordering::SeqCst), 1, "one execution owner");
    assert_eq!(
        outcome
            .context()
            .tasks
            .iter()
            .filter(|t| t.digest().is_some())
            .count(),
        2
    );
}

#[test]
fn one_journal_takes_one_writer() {
    let fixture = Fixture::new("writer");
    let first = fixture.store();
    let second = fixture.store();
    first
        .append_simple(Kind::User, "one", 1, 1)
        .expect("first writer appends");

    // The second store's cursor is now stale: it refuses instead of interleaving
    // an event whose id would collide with the first writer's.
    let refused = second.append_simple(Kind::User, "two", 1, 2);
    assert!(
        refused
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("outside this Store")),
        "{refused:?}"
    );
    // Nothing was written by the refused append, and the first writer's fact
    // is still the whole journal.
    assert_eq!(first.events().expect("events").len(), 1);
}
