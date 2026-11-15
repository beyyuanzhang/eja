#![allow(clippy::significant_drop_tightening)] // a guard held for one test block is contention-free
//! Composition laws: these cross the boundaries that isolated feature tests miss.
mod support;
use eja::{
    adapters::{render_context_within, Capabilities, Ports},
    *,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use support::{
    aja, control, group, link, task, turn_at, turn_with, Done, Script, State as Fixture,
};

/// The one decision this file's brain makes: it says "finished" and does the actions.
fn script(actions: Vec<Action>) -> Vec<Decision> {
    vec![Decision {
        reply: "finished".into(),
        actions,
        ..Decision::default()
    }]
}

/// A worker that records every assignment, so a test can state what it ran.
#[derive(Default)]
struct WorkerLog(Mutex<Vec<Assignment>>);
impl Worker for WorkerLog {
    fn execute(&self, a: &Assignment, _: &eja::Control) -> String {
        self.0.lock().unwrap().push(a.clone());
        "ended: message\nwork recorded".into()
    }
}

fn close(id: u64, accepted: bool) -> Action {
    Action::CloseTask {
        task_id: id,
        accepted,
        note: "judged".into(),
    }
}

#[test]
fn torn_tail_recovery_is_closed_under_future_appends_and_committed_corruption_is_an_error() {
    use std::io::Write;
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    store.append_simple(Kind::User, "one", 1, 1).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&store.log_path)
        .unwrap()
        .write_all(b"{\"id\":")
        .unwrap();
    let restarted = fixture.store();
    restarted.append_simple(Kind::User, "two", 2, 2).unwrap();
    restarted.append_simple(Kind::User, "three", 3, 3).unwrap();
    assert_eq!(
        fixture
            .store()
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
    assert!(Store::new(fixture.path()).is_err());
}

#[test]
fn approval_is_a_host_fact_and_one_atomic_identity_event_resolves_the_proposal() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let proposed = turn_at(
        &store,
        "request",
        script(vec![Action::Propose {
            kind: ProposalKind::Goal,
            target_id: None,
            text: "goal".into(),
        }]),
    );
    let id = proposed.context().proposals[0].id;
    let refused = turn_at(
        &store,
        "request",
        script(vec![Action::Confirm { proposal_id: id }]),
    );
    assert!(refused.context().goal.is_none());
    let options = TurnOptions {
        approvals: vec![id],
        ..Default::default()
    };
    let confirmed = turn_at_with_options(
        &store,
        "explicit approval",
        &Script::new(script(vec![Action::Confirm { proposal_id: id }])),
        &Done::default(),
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert!(confirmed.context().proposals.is_empty());
    assert_eq!(confirmed.context().goal.unwrap().text, "goal");
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
    // An approval is its own fact, not a field the caller re-declares each turn.
    assert!(events.iter().any(|event| event.kind == Kind::Approval
        && event
            .data
            .as_ref()
            .is_some_and(|data| data["proposal_id"] == json!(id))));
    assert!(!events
        .iter()
        .any(|event| event.kind == Kind::User && event.data.is_some()));
    let state = State::load(&fixture.store()).expect("state");
    assert!(state.project("", None, now_utc()).proposals.is_empty());
}

#[test]
fn an_approval_cannot_confirm_a_proposal_in_its_creation_turn() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    // Event 1 is the user event, so the proposal a scripted decision creates is
    // event 2: the decision may name it, and the same turn may not confirm it.
    let out = turn_at(
        &store,
        "request",
        script(vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "later".into(),
            },
            Action::Confirm { proposal_id: 2 },
        ]),
    );
    assert!(out.context().commitments.is_empty());
    assert!(out
        .context()
        .refused
        .iter()
        .any(|note| note.contains("created this turn")));
}

#[test]
fn groups_close_from_their_members_without_a_fake_execution() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let worker = WorkerLog::default();
    turn_with(
        &store,
        "request",
        script(vec![
            group("job"),
            aja("child"),
            link(2, 1, RelationKind::PartOf),
        ]),
        &worker,
    );
    let premature = turn_with(&store, "request", script(vec![close(1, true)]), &worker);
    assert_eq!(premature.context().tasks[0].status, TaskStatus::Open);
    let out = turn_with(
        &store,
        "request",
        script(vec![close(2, true), close(1, true)]),
        &worker,
    );
    assert!(out.context().refused.is_empty());
    assert!(out
        .context()
        .tasks
        .iter()
        .all(|task| task.status == TaskStatus::Done));
    assert_eq!(worker.0.lock().unwrap().len(), 1);
    let refused = turn_with(
        &store,
        "request",
        script(vec![Action::Dispatch { task_id: 1 }]),
        &worker,
    );
    assert!(!refused.context().refused.is_empty());
}

#[test]
fn graph_constraints_include_cross_kind_cycles_and_real_dependency_readiness() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let out = turn_at(
        &store,
        "request",
        script(vec![
            group("job"),
            task("a"),
            task("b"),
            link(2, 1, RelationKind::PartOf),
            link(2, 1, RelationKind::DependsOn),
            link(3, 2, RelationKind::DependsOn),
            link(2, 3, RelationKind::DependsOn),
            link(2, 999, RelationKind::RelatesTo),
            link(2, 2, RelationKind::RelatesTo),
        ]),
    );
    assert_eq!(out.context().relations.len(), 2);
    // Four actions are refused; two of them are the same cause (a cycle through
    // the group), and a refusal is told once per distinct note.
    assert_eq!(out.context().refused.len(), 3);
    assert_eq!(
        out.context()
            .refused
            .iter()
            .filter(|note| note.contains("dependency cycle"))
            .count(),
        1
    );
    let blocked = turn_at(
        &store,
        "request",
        script(vec![Action::Dispatch { task_id: 3 }]),
    );
    assert!(blocked.dispatched.is_empty());
    turn_at(
        &store,
        "request",
        script(vec![Action::Dispatch { task_id: 2 }]),
    );
    turn_at(&store, "request", script(vec![close(2, true)]));
    let ready = turn_at(
        &store,
        "request",
        script(vec![Action::Dispatch { task_id: 3 }]),
    );
    assert_eq!(ready.dispatched, vec![3]);
}

#[test]
fn continuation_has_one_predecessor_and_cannot_be_changed_after_dispatch() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let worker = WorkerLog::default();
    turn_with(
        &store,
        "request",
        script(vec![aja("a"), aja("b"), task("next")]),
        &worker,
    );
    let out = turn_with(
        &store,
        "request",
        script(vec![
            link(3, 1, RelationKind::Continues),
            link(3, 2, RelationKind::Continues),
            Action::Dispatch { task_id: 3 },
            link(3, 2, RelationKind::Continues),
        ]),
        &worker,
    );
    assert_eq!(
        out.context()
            .relations
            .iter()
            .filter(|edge| edge.from == 3)
            .count(),
        1
    );
    assert_eq!(worker.0.lock().unwrap().last().unwrap().resume, Some(1));
    assert_eq!(out.context().refused.len(), 2);
}

struct World;
impl Capabilities for World {
    fn observe(&self, _: &str, _: &Value, _: &eja::Control) -> Result<Value> {
        Ok(json!({"fact":7}))
    }
}
fn look() -> Action {
    Action::Look {
        target: "fact".into(),
        arguments: Value::Null,
        selector: Selector::Whole,
    }
}
#[test]
fn unrelated_tasks_do_not_change_world_capabilities() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let ports = Ports {
        capabilities: Some(&World),
        ..Default::default()
    };
    let first = turn(
        &store,
        "read",
        &Script::new(script(vec![look()])),
        &Done::default(),
        &ports,
    )
    .unwrap();
    turn_at(&store, "request", script(vec![aja("unrelated")]));
    let second = turn(
        &store,
        "read",
        &Script::new(script(vec![look()])),
        &Done::default(),
        &ports,
    )
    .unwrap();
    // The same observation is available after an unrelated task: the capability
    // does not depend on the task index.
    assert_eq!(first.context().evidence, second.context().evidence);
    assert_eq!(first.context().evidence.len(), 1);
    assert!(first.context().refused.is_empty() && second.context().refused.is_empty());
}

struct Growing;
impl Brain for Growing {
    fn decide(&self, _: &EjaContext<'_>, _: &eja::Control) -> Decision {
        Decision {
            actions: vec![aja("another")],
            ..Decision::default()
        }
    }
}
#[test]
fn endlessly_novel_states_stop_at_the_turn_budget_with_all_records_retained() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
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
        &Done::default(),
        &Ports::default(),
        now_utc(),
        &options,
    )
    .unwrap();
    assert_eq!(out.end, TurnEnd::Budget);
    assert_eq!(out.decisions, 3);
    assert_eq!(out.dispatched.len(), 3);
    assert!(out
        .context()
        .tasks
        .iter()
        .all(|task| task.status == TaskStatus::Waiting));
    assert!(out.reply.contains("budget"));
}
struct Wait;
impl Brain for Wait {
    fn decide(&self, _: &EjaContext<'_>, control: &Control) -> Decision {
        while control.check().is_ok() {
            std::thread::sleep(Duration::from_millis(1));
        }
        Decision::default()
    }
}
#[test]
fn turn_deadline_and_cancellation_cross_the_brain_boundary() {
    let parent = Control::new(Duration::from_secs(5), std::sync::Arc::default());
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
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
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
        &Done::default(),
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
        &Done::default(),
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
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    turn_at(&store, "remember this fact", script(vec![]));
    let state = State::load(&store).expect("state");
    let context = state.project("next", None, now_utc());
    assert!(context
        .history
        .iter()
        .any(|event| event.text == "remember this fact"));
    assert!(context.history.iter().any(|event| event.text == "finished"));
    let history = Ports::default()
        .look(
            "log:history",
            &Value::Null,
            &Selector::Range { from: 0, lines: 2 },
            &store.events().unwrap(),
            &control(),
        )
        .unwrap();
    assert_eq!(history["events"].as_array().unwrap().len(), 2);
    let tall = "汉字\"\n".repeat(2000);
    let context = state.project(&tall, None, now_utc());
    for budget in [0, 1, 2, 3, 64, 400, 2000] {
        let view = render_context_within(&context, budget);
        assert!(view.len() <= budget);
    }
}

#[test]
fn a_stale_store_refuses_to_overwrite_another_writers_cursor() {
    let fixture = Fixture::new("convergence");
    let first = fixture.store();
    let second = fixture.store();
    first.append_simple(Kind::User, "first", 1, 1).unwrap();
    assert!(second.append_simple(Kind::User, "stale", 1, 1).is_err());
    let fresh = fixture.store();
    let next = fresh.append_simple(Kind::User, "second", 2, 2).unwrap();
    assert_eq!(next.id, 2);
    assert_eq!(fresh.events().unwrap().len(), 2);
}

#[test]
fn cloned_store_appends_keep_a_single_monotone_event_order() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
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
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let worker = WorkerLog::default();
    turn_with(
        &store,
        "request",
        script(vec![
            group("best effort"),
            task("optional check"),
            link(2, 1, RelationKind::PartOf),
            close(2, false),
        ]),
        &worker,
    );
    let out = turn_with(&store, "request", script(vec![close(1, true)]), &worker);
    assert_eq!(out.context().tasks[0].status, TaskStatus::Done);
    assert_eq!(out.context().tasks[1].status, TaskStatus::Failed);
    assert!(out.context().refused.is_empty());
    let empty = turn_with(
        &store,
        "request",
        script(vec![group("empty selection"), close(3, true)]),
        &worker,
    );
    assert_eq!(empty.context().tasks[2].status, TaskStatus::Done);
    assert!(worker.0.lock().unwrap().is_empty());
}

struct HeldBrain {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Brain for HeldBrain {
    fn decide(&self, _: &EjaContext<'_>, _: &eja::Control) -> Decision {
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
        let fixture = Fixture::new("convergence");
        let first = fixture.store();
        let second = if cloned {
            first.clone()
        } else {
            fixture.store()
        };
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
                &Done::default(),
                &Ports::default(),
            )
            .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let blocked = turn(
            &second,
            "second",
            &Script::new(script(vec![])),
            &Done::default(),
            &Ports::default(),
        );
        assert!(blocked.unwrap_err().to_string().contains("another turn"));
        release_tx.send(()).unwrap();
        assert_eq!(running.join().unwrap().end, TurnEnd::Complete);
    }
}

struct ReceiptBrain(Mutex<usize>);
impl Brain for ReceiptBrain {
    fn decide(&self, context: &EjaContext<'_>, _: &eja::Control) -> Decision {
        let mut step = self.0.lock().unwrap();
        let actions = match *step {
            0 => vec![task("allocated")],
            1 => vec![Action::Dispatch {
                task_id: context.focus.expect("creation returns focus"),
            }],
            2 => vec![close(context.focus.unwrap(), true)],
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
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let out = turn(
        &store,
        "execute",
        &ReceiptBrain(Mutex::new(0)),
        &Done::default(),
        &Ports::default(),
    )
    .unwrap();
    assert_eq!(out.decisions, 3);
    assert_eq!(out.context().tasks[0].status, TaskStatus::Done);
    assert!(out.context().refused.is_empty());
}

#[test]
fn oversized_log_events_do_not_block_discovery_and_remain_readable_by_utf8_offset() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let first = store
        .append_simple(Kind::User, &"前\n\"后".repeat(80000), 1, 1)
        .unwrap();
    store.append_simple(Kind::User, "later fact", 1, 2).unwrap();
    let events = store.events().unwrap();
    let ports = Ports::default();
    let page = ports
        .look(
            "log:all",
            &Value::Null,
            &Selector::Range { from: 0, lines: 2 },
            &events,
            &control(),
        )
        .unwrap();
    assert_eq!(page["next"], 2);
    assert_eq!(page["partial_events"][0]["id"], first.id);
    assert_eq!(page["events"][0]["text"], "later fact");
    // The frame cap bounds the serialized page, so measure it as sent.
    assert!(page.to_string().len() <= eja::aja::FRAME_LIMIT);
    let target = format!("log:event:{}", first.id);
    let text = serde_json::to_string(&first).unwrap();
    let at = text.find('前').unwrap();
    let chunk = ports
        .look(
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
        &Script::new(script(vec![Action::Look {
            target: "log:all".into(),
            arguments: Value::Null,
            selector: Selector::Range { from: 0, lines: 2 },
        }])),
        &Done::default(),
        &ports,
    )
    .unwrap();
    let rendered = eja::adapters::render_context(&out.context());
    assert!(rendered.contains("next=2"));
    assert!(rendered.contains("oversized events"));
}

struct CancelLookup(Arc<AtomicBool>);
impl eja::Host for CancelLookup {
    fn look(&self, _: &str, _: &Value, _: &Selector, _: &[Event], _: &Control) -> Result<Value> {
        self.0.store(true, Ordering::SeqCst);
        Err(ControlError::Cancelled.into())
    }
}
#[test]
fn cancellation_keeps_prior_refusals_and_reconciles_reserved_dispatches() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let cancelled = Arc::new(AtomicBool::new(false));
    let out = turn_at_with_options(
        &store,
        "run",
        &Script::new(script(vec![
            Action::Dispatch { task_id: 999 },
            aja("reserved"),
            look(),
        ])),
        &Done::default(),
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
    assert_eq!(out.context().tasks[0].status, TaskStatus::Waiting);
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
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker = CancelAfterFirst {
        cancelled: cancelled.clone(),
        seen: Mutex::new(Vec::new()),
    };
    let out = turn_at_with_options(
        &store,
        "two ajas",
        &Script::new(script(vec![aja("first"), aja("second")])),
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
        out.context().tasks[0].digest(),
        Some("first execution completed")
    );
    assert!(out.context().tasks[1]
        .digest()
        .unwrap()
        .contains("interrupted"));
    assert!(out
        .context()
        .tasks
        .iter()
        .all(|task| task.status == TaskStatus::Waiting));
}
#[test]
fn a_rejection_is_recorded_and_the_proposal_leaves_the_view() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let proposed = turn_at(
        &store,
        "request",
        script(vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "later".into(),
        }]),
    );
    let id = proposed.context().proposals[0].id;
    let rejected = turn_at(
        &store,
        "request",
        script(vec![Action::Reject {
            proposal_id: id,
            note: "not now".into(),
        }]),
    );
    assert!(
        rejected.context().proposals.is_empty(),
        "a rejected proposal is no longer pending"
    );
    let events = store.events().unwrap();
    let rejection = events
        .iter()
        .find(|event| event.kind == Kind::ProposalRejected)
        .expect("a rejection is a fact");
    let data = rejection.data.as_ref().expect("data");
    assert_eq!(data["proposal_id"], json!(id));
    assert_eq!(data["note"], json!("not now"));
    // Rejecting twice is refused: the fact is already resolved.
    let again = turn_at(
        &store,
        "request",
        script(vec![Action::Reject {
            proposal_id: id,
            note: "again".into(),
        }]),
    );
    assert!(
        again
            .context()
            .refused
            .iter()
            .any(|note| note.contains("not pending")),
        "{:?}",
        again.context().refused
    );
}

#[test]
fn a_recorded_relationship_is_identity_content() {
    let fixture = Fixture::new("convergence");
    let store = fixture.store();
    let text = "the bench answers to the scheduler";
    turn_at(
        &store,
        "request",
        script(vec![Action::RecordRelationship { text: text.into() }]),
    );
    let events = store.events().unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == Kind::Relationship && event.text == text));
    let state = State::load(&store).expect("state");
    let context = state.project("", None, now_utc());
    assert!(
        context.identity.iter().any(|event| event.text == text),
        "a relationship is identity content, not a task edge"
    );
    let view = render_context_within(&context, usize::MAX);
    assert!(view.contains("ACTIVE IDENTITY"), "{view}");
    assert!(view.contains(text), "{view}");
}
