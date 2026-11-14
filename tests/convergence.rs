//! Composition laws: these cross the boundaries that isolated feature tests miss.
mod support;
use eja::{
    adapters::{render_context_within, Observer, Ports},
    *,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use support::control;
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "eja-laws-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn store(&self) -> Store {
        Store::new(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Script(Mutex<VecDeque<Decision>>);
impl Brain for Script {
    fn decide(&self, _: &EjaContext, _: &eja::Control) -> Decision {
        self.0.lock().unwrap().pop_front().unwrap_or_default()
    }
}
fn script(actions: Vec<Action>) -> Script {
    Script(Mutex::new(
        vec![Decision {
            reply: "finished".into(),
            actions,
            ..Decision::default()
        }]
        .into(),
    ))
}
#[derive(Default)]
struct WorkerLog(Mutex<Vec<Assignment>>);
impl Worker for WorkerLog {
    fn execute(&self, a: &Assignment, _: &eja::Control) -> String {
        self.0.lock().unwrap().push(a.clone());
        "ended: message\nwork recorded".into()
    }
}
fn aja(title: &str, run: bool) -> Action {
    Action::OpenTask {
        title: title.into(),
        request: title.into(),
        acceptance: "done".into(),
        run,
    }
}
fn edge(from: u64, to: u64, kind: RelationKind) -> Action {
    Action::LinkTasks { from, to, kind }
}
fn close(id: u64, accepted: bool) -> Action {
    Action::CloseTask {
        task_id: id,
        accepted,
        note: "judged".into(),
    }
}
fn run(store: &Store, actions: Vec<Action>, worker: &WorkerLog) -> TurnOutcome {
    turn(
        store,
        "request",
        &script(actions),
        worker,
        &Ports::default(),
    )
    .unwrap()
}

#[test]
fn torn_tail_recovery_is_closed_under_future_appends_and_committed_corruption_is_an_error() {
    use std::io::Write;
    let f = Fixture::new();
    let store = f.store();
    store.append_simple(Kind::User, "one", 1, 1).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&store.log_path)
        .unwrap()
        .write_all(b"{\"id\":")
        .unwrap();
    let restarted = f.store();
    restarted.append_simple(Kind::User, "two", 2, 2).unwrap();
    restarted.append_simple(Kind::User, "three", 3, 3).unwrap();
    assert_eq!(
        f.store()
            .events()
            .unwrap()
            .iter()
            .map(|event| event.text.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "two", "three"]
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&store.log_path)
        .unwrap()
        .write_all(b"not json\n")
        .unwrap();
    assert!(Store::new(&f.0).is_err());
}

#[test]
fn approval_is_a_host_fact_and_one_atomic_identity_event_resolves_the_proposal() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    let proposed = run(
        &store,
        vec![Action::Propose {
            kind: ProposalKind::Goal,
            target_id: None,
            text: "goal".into(),
        }],
        &worker,
    );
    let id = proposed.context.proposals[0].id;
    let refused = run(&store, vec![Action::Confirm { proposal_id: id }], &worker);
    assert!(refused.context.goal.is_none());
    let options = TurnOptions {
        approvals: vec![id],
        ..Default::default()
    };
    let confirmed = turn_at_with_options(
        &store,
        "explicit approval",
        &script(vec![Action::Confirm { proposal_id: id }]),
        &worker,
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert!(confirmed.context.proposals.is_empty());
    assert_eq!(confirmed.context.goal.unwrap().text, "goal");
    let events = store.events().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == Kind::Goal)
            .count(),
        1
    );
    assert!(!events
        .iter()
        .any(|event| event.kind == Kind::ProposalConfirmed));
    assert!(events.iter().any(|event| event.kind == Kind::User
        && event
            .data
            .as_ref()
            .is_some_and(|data| data["approved_proposals"] == json!([id]))));
    assert!(project(&f.store(), "", None, now_utc())
        .unwrap()
        .proposals
        .is_empty());
}

#[test]
fn prospective_host_approval_cannot_confirm_a_proposal_in_its_creation_turn() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    let options = TurnOptions {
        approvals: vec![2],
        ..Default::default()
    };
    let out = turn_at_with_options(
        &store,
        "request",
        &script(vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "later".into(),
            },
            Action::Confirm { proposal_id: 2 },
        ]),
        &worker,
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert!(out.context.commitments.is_empty());
    assert!(out
        .refused
        .iter()
        .any(|note| note.contains("created this turn")));
}

#[test]
fn groups_close_from_their_members_without_a_fake_execution() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    run(
        &store,
        vec![
            Action::OpenGroup {
                title: "job".into(),
                acceptance: "children done".into(),
            },
            aja("child", true),
            edge(2, 1, RelationKind::PartOf),
        ],
        &worker,
    );
    let premature = run(&store, vec![close(1, true)], &worker);
    assert_eq!(premature.tasks[0].status, TaskStatus::Open);
    let out = run(&store, vec![close(2, true), close(1, true)], &worker);
    assert!(out.refused.is_empty());
    assert!(out.tasks.iter().all(|task| task.status == TaskStatus::Done));
    assert_eq!(worker.0.lock().unwrap().len(), 1);
    let refused = run(&store, vec![Action::Dispatch { task_id: 1 }], &worker);
    assert!(!refused.refused.is_empty());
}

#[test]
fn graph_constraints_include_cross_kind_cycles_and_real_dependency_readiness() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    let out = run(
        &store,
        vec![
            Action::OpenGroup {
                title: "job".into(),
                acceptance: "children".into(),
            },
            aja("a", false),
            aja("b", false),
            edge(2, 1, RelationKind::PartOf),
            edge(2, 1, RelationKind::DependsOn),
            edge(3, 2, RelationKind::DependsOn),
            edge(2, 3, RelationKind::DependsOn),
            edge(2, 999, RelationKind::RelatesTo),
            edge(2, 2, RelationKind::RelatesTo),
        ],
        &worker,
    );
    assert_eq!(out.context.relations.len(), 2);
    assert_eq!(out.refused.len(), 4);
    let blocked = run(&store, vec![Action::Dispatch { task_id: 3 }], &worker);
    assert!(blocked.dispatched.is_empty());
    run(&store, vec![Action::Dispatch { task_id: 2 }], &worker);
    run(&store, vec![close(2, true)], &worker);
    let ready = run(&store, vec![Action::Dispatch { task_id: 3 }], &worker);
    assert_eq!(ready.dispatched, vec![3]);
}

#[test]
fn continuation_has_one_predecessor_and_cannot_be_changed_after_dispatch() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    run(
        &store,
        vec![aja("a", true), aja("b", true), aja("next", false)],
        &worker,
    );
    let out = run(
        &store,
        vec![
            edge(3, 1, RelationKind::Continues),
            edge(3, 2, RelationKind::Continues),
            Action::Dispatch { task_id: 3 },
            edge(3, 2, RelationKind::Continues),
        ],
        &worker,
    );
    assert_eq!(
        out.context
            .relations
            .iter()
            .filter(|edge| edge.from == 3)
            .count(),
        1
    );
    assert_eq!(worker.0.lock().unwrap().last().unwrap().resume, Some(1));
    assert_eq!(out.refused.len(), 2);
}

struct World;
impl Observer for World {
    fn observe(&self, _: &str, _: &Value, _: &eja::Control) -> Result<Value> {
        Ok(json!({"fact":7}))
    }
}
fn look() -> Action {
    Action::Look {
        source: LookSource::World,
        target: "fact".into(),
        arguments: Value::Null,
        selector: Selector::Whole,
    }
}
#[test]
fn unrelated_tasks_do_not_change_world_capabilities() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    let ports = Ports {
        world: Some(&World),
        ..Default::default()
    };
    let first = turn(&store, "read", &script(vec![look()]), &worker, &ports).unwrap();
    run(&store, vec![aja("unrelated", true)], &worker);
    let second = turn(&store, "read", &script(vec![look()]), &worker, &ports).unwrap();
    assert_eq!(first.evidence, second.evidence);
    assert!(first.refused.is_empty() && second.refused.is_empty());
}

struct Growing;
impl Brain for Growing {
    fn decide(&self, _: &EjaContext, _: &eja::Control) -> Decision {
        Decision {
            actions: vec![aja("another", true)],
            ..Decision::default()
        }
    }
}
#[test]
fn endlessly_novel_states_stop_at_the_turn_budget_with_all_records_retained() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    let options = TurnOptions {
        limits: TurnLimits {
            decisions: 3,
            ..Default::default()
        },
        ..Default::default()
    };
    let out = turn_at_with_options(
        &store,
        "keep going",
        &Growing,
        &worker,
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert_eq!(out.end, TurnEnd::Budget);
    assert_eq!(out.decisions, 3);
    assert_eq!(out.dispatched.len(), 3);
    assert!(out
        .tasks
        .iter()
        .all(|task| task.status == TaskStatus::Waiting));
    assert!(out.reply.contains("budget"));
}
struct Wait;
impl Brain for Wait {
    fn decide(&self, _: &EjaContext, control: &Control) -> Decision {
        while control.check().is_ok() {
            std::thread::sleep(Duration::from_millis(1));
        }
        Decision::default()
    }
}
#[test]
fn turn_deadline_and_cancellation_cross_the_brain_boundary() {
    let parent = Control::new(Duration::from_secs(5), Default::default());
    let local = Arc::new(AtomicBool::new(false));
    let child = parent
        .child(Duration::from_secs(50))
        .cancelled_by(local.clone());
    assert!(child.remaining().unwrap() <= Duration::from_secs(5));
    local.store(true, Ordering::SeqCst);
    assert_eq!(child.check(), Err(ControlError::Cancelled));
    assert!(
        parent.check().is_ok(),
        "child revocation does not revoke its parent"
    );
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    let options = TurnOptions {
        limits: TurnLimits {
            timeout_ms: 20,
            ..Default::default()
        },
        ..Default::default()
    };
    let started = Instant::now();
    let out = turn_at_with_options(
        &store,
        "wait",
        &Wait,
        &worker,
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert_eq!(out.end, TurnEnd::Budget);
    assert!(started.elapsed() < Duration::from_secs(1));
    let options = TurnOptions {
        cancelled: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    };
    let out = turn_at_with_options(
        &store,
        "cancelled",
        &Growing,
        &worker,
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert_eq!(out.end, TurnEnd::Cancelled);
    assert_eq!(out.decisions, 0);
}

#[test]
fn history_is_discoverable_and_all_rendered_views_obey_utf8_byte_budgets() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    turn(
        &store,
        "remember this fact",
        &script(vec![]),
        &worker,
        &Ports::default(),
    )
    .unwrap();
    let context = project(&store, "next", None, now_utc()).unwrap();
    assert!(context
        .history
        .iter()
        .any(|event| event.text == "remember this fact"));
    assert!(context.history.iter().any(|event| event.text == "finished"));
    let history = Ports::default()
        .look(
            LookSource::Log,
            "history",
            &Value::Null,
            &Selector::Range { from: 0, lines: 2 },
            &store.events().unwrap(),
            &control(),
        )
        .unwrap();
    assert_eq!(history["events"].as_array().unwrap().len(), 2);
    let mut context = context;
    context.input = "汉字\"\n".repeat(2000);
    for budget in [0, 1, 2, 3, 64, 400, 2000] {
        let view = render_context_within(&context, budget);
        assert!(view.len() <= budget);
    }
}

#[test]
fn a_stale_store_refuses_to_overwrite_another_writers_cursor() {
    let f = Fixture::new();
    let first = f.store();
    let second = f.store();
    first.append_simple(Kind::User, "first", 1, 1).unwrap();
    assert!(second.append_simple(Kind::User, "stale", 1, 1).is_err());
    let fresh = f.store();
    let next = fresh.append_simple(Kind::User, "second", 2, 2).unwrap();
    assert_eq!(next.id, 2);
    assert_eq!(fresh.events().unwrap().len(), 2);
}

#[test]
fn cloned_store_appends_keep_a_single_monotone_event_order() {
    let f = Fixture::new();
    let store = f.store();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let store = store.clone();
            scope.spawn(move || {
                for _ in 0..10 {
                    store.append_simple(Kind::User, "event", 1, 1).unwrap();
                }
            });
        }
    });
    let events = store.events().unwrap();
    assert_eq!(events.len(), 40);
    assert!(events.windows(2).all(|pair| pair[0].id + 1 == pair[1].id));
}

#[test]
fn group_verdict_is_not_a_second_quality_authority_over_eja() {
    let f = Fixture::new();
    let store = f.store();
    let worker = WorkerLog::default();
    run(
        &store,
        vec![
            Action::OpenGroup {
                title: "best effort".into(),
                acceptance: "report even tolerated failures".into(),
            },
            aja("optional check", false),
            edge(2, 1, RelationKind::PartOf),
            close(2, false),
        ],
        &worker,
    );
    let out = run(&store, vec![close(1, true)], &worker);
    assert_eq!(out.tasks[0].status, TaskStatus::Done);
    assert_eq!(out.tasks[1].status, TaskStatus::Failed);
    assert!(out.refused.is_empty());
    let empty = run(
        &store,
        vec![
            Action::OpenGroup {
                title: "empty selection".into(),
                acceptance: "no work required".into(),
            },
            close(3, true),
        ],
        &worker,
    );
    assert_eq!(empty.tasks[2].status, TaskStatus::Done);
    assert!(worker.0.lock().unwrap().is_empty());
}

struct HeldBrain {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Brain for HeldBrain {
    fn decide(&self, _: &EjaContext, _: &eja::Control) -> Decision {
        self.entered.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        Decision::default()
    }
}
#[test]
fn the_journal_lease_excludes_both_cloned_and_independent_stores() {
    for cloned in [false, true] {
        let f = Fixture::new();
        let first = f.store();
        let second = if cloned { first.clone() } else { f.store() };
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let running = std::thread::spawn(move || {
            turn(
                &first,
                "first",
                &HeldBrain {
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                },
                &WorkerLog::default(),
                &Ports::default(),
            )
            .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let blocked = turn(
            &second,
            "second",
            &script(vec![]),
            &WorkerLog::default(),
            &Ports::default(),
        );
        assert!(blocked.unwrap_err().to_string().contains("another turn"));
        release_tx.send(()).unwrap();
        assert_eq!(running.join().unwrap().end, TurnEnd::Complete);
    }
}

struct ReceiptBrain(Mutex<usize>);
impl Brain for ReceiptBrain {
    fn decide(&self, context: &EjaContext, _: &eja::Control) -> Decision {
        let mut step = self.0.lock().unwrap();
        let actions = match *step {
            0 => vec![aja("allocated", false)],
            1 => vec![Action::Dispatch {
                task_id: context.focus.as_ref().expect("creation returns focus").id,
            }],
            2 => vec![close(context.focus.as_ref().unwrap().id, true)],
            _ => panic!("unexpected decision"),
        };
        *step += 1;
        Decision {
            reply: "finished".into(),
            actions,
            ..Decision::default()
        }
    }
}
#[test]
fn allocation_returns_an_id_before_the_model_has_to_address_the_new_entity() {
    let f = Fixture::new();
    let store = f.store();
    let out = turn(
        &store,
        "execute",
        &ReceiptBrain(Mutex::new(0)),
        &WorkerLog::default(),
        &Ports::default(),
    )
    .unwrap();
    assert_eq!(out.decisions, 3);
    assert_eq!(out.tasks[0].status, TaskStatus::Done);
    assert!(out.refused.is_empty());
}

#[test]
fn oversized_log_events_do_not_block_discovery_and_remain_readable_by_utf8_offset() {
    let f = Fixture::new();
    let store = f.store();
    let first = store
        .append_simple(Kind::User, &"前\n\"后".repeat(80000), 1, 1)
        .unwrap();
    store.append_simple(Kind::User, "later fact", 1, 2).unwrap();
    let events = store.events().unwrap();
    let ports = Ports::default();
    let page = ports
        .look(
            LookSource::Log,
            "all",
            &Value::Null,
            &Selector::Range { from: 0, lines: 2 },
            &events,
            &control(),
        )
        .unwrap();
    assert_eq!(page["next"], 2);
    assert_eq!(page["partial_events"][0]["id"], first.id);
    assert_eq!(page["events"][0]["text"], "later fact");
    assert!(page.to_string().len() <= aja::FRAME_LIMIT);
    let target = format!("event:{}", first.id);
    let text = serde_json::to_string(&first).unwrap();
    let at = text.find('前').unwrap();
    let chunk = ports
        .look(
            LookSource::Log,
            &target,
            &Value::Null,
            &Selector::Bytes { from: at, bytes: 1 },
            &events,
            &control(),
        )
        .unwrap();
    assert_eq!(chunk["text"], "前");
    assert_eq!(chunk["next_offset"], at + 3);
    let out = turn(
        &store,
        "read",
        &script(vec![Action::Look {
            source: LookSource::Log,
            target: "all".into(),
            arguments: Value::Null,
            selector: Selector::Range { from: 0, lines: 2 },
        }]),
        &WorkerLog::default(),
        &ports,
    )
    .unwrap();
    let rendered = eja::adapters::render_context(&out.context);
    assert!(rendered.contains("next=2"));
    assert!(rendered.contains("oversized events"));
}

struct CancelLookup(Arc<AtomicBool>);
impl Lookup for CancelLookup {
    fn look(
        &self,
        _: LookSource,
        _: &str,
        _: &Value,
        _: &Selector,
        _: &[Event],
        _: &Control,
    ) -> Result<Value> {
        self.0.store(true, Ordering::SeqCst);
        Err(ControlError::Cancelled.into())
    }
}
#[test]
fn cancellation_keeps_prior_refusals_and_reconciles_reserved_dispatches() {
    let f = Fixture::new();
    let store = f.store();
    let cancelled = Arc::new(AtomicBool::new(false));
    let out = turn_at_with_options(
        &store,
        "run",
        &script(vec![
            Action::Dispatch { task_id: 999 },
            aja("reserved", true),
            look(),
        ]),
        &WorkerLog::default(),
        &CancelLookup(cancelled.clone()),
        now_utc(),
        &TurnOptions {
            cancelled,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.end, TurnEnd::Cancelled);
    assert!(store
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == Kind::Refused && e.text.contains("999")));
    assert_eq!(out.tasks[0].status, TaskStatus::Waiting);
    assert!(out.dispatched.is_empty());
}

#[test]
fn cancellation_between_queued_ajas_preserves_completed_evidence_and_skips_the_next_run() {
    struct CancelAfterFirst {
        cancelled: Arc<AtomicBool>,
        seen: Mutex<Vec<u64>>,
    }
    impl Worker for CancelAfterFirst {
        fn execute(&self, assignment: &Assignment, _: &Control) -> String {
            self.seen.lock().unwrap().push(assignment.task_id);
            self.cancelled.store(true, Ordering::SeqCst);
            "first execution completed".into()
        }
    }
    let f = Fixture::new();
    let store = f.store();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = CancelAfterFirst {
        cancelled: cancelled.clone(),
        seen: Mutex::new(Vec::new()),
    };
    let out = turn_at_with_options(
        &store,
        "two ajas",
        &script(vec![aja("first", true), aja("second", true)]),
        &worker,
        &Ports::default(),
        now_utc(),
        &TurnOptions {
            cancelled,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.end, TurnEnd::Cancelled);
    assert_eq!(*worker.seen.lock().unwrap(), vec![1]);
    assert_eq!(out.dispatched, vec![1]);
    assert_eq!(
        out.tasks[0].digest.as_deref(),
        Some("first execution completed")
    );
    assert!(out.tasks[1]
        .digest
        .as_deref()
        .unwrap()
        .contains("interrupted"));
    assert!(out
        .tasks
        .iter()
        .all(|task| task.status == TaskStatus::Waiting));
}
