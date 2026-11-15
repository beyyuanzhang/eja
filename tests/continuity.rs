//! Continuity: the facts a later turn still reads, and the one clock they carry.

mod support;

use chrono::{TimeZone, Utc};
use eja::adapters::Ports;
use eja::{
    Action, Assignment, Decision, Kind, ProposalKind, State, Store, TaskStatus, TurnOptions,
    TurnOutcome,
};
use serde::Deserialize;
use support::{aja, decision, Done, Script};

fn day(year: i32, month: u32, day: u32, hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
        .single()
        .expect("valid UTC time")
}

/// One turn at a stated instant. The kit's turns read the host clock; continuity
/// is about what a later turn reads, so this file states the time instead.
fn turn_at_time(
    store: &Store,
    input: &'static str,
    decisions: Vec<Decision>,
    at: chrono::DateTime<Utc>,
) -> TurnOutcome<'static> {
    eja::turn_at(
        store,
        input,
        &Script::new(decisions),
        &Done::default(),
        &Ports::default(),
        at,
    )
    .expect("turn")
}

fn propose_and_confirm(
    store: &Store,
    kind: ProposalKind,
    text: &str,
    target_id: Option<u64>,
    at: chrono::DateTime<Utc>,
) {
    let outcome = turn_at_time(
        store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind,
            target_id,
            text: text.to_string(),
        }])],
        at,
    );
    let proposal_id = outcome
        .context()
        .proposals
        .last()
        .expect("pending proposal")
        .id;
    eja::turn_at_with_options(
        store,
        "confirm",
        &Script::new(vec![decision(vec![Action::Confirm { proposal_id }])]),
        &Done::default(),
        &Ports::default(),
        at,
        &TurnOptions {
            approvals: vec![proposal_id],
            ..Default::default()
        },
    )
    .expect("confirm");
}

#[test]
fn user_input_does_not_become_a_goal() {
    let state = support::State::new("continuity-input");
    let outcome = turn_at_time(
        &state.store(),
        "what is the weather?",
        vec![decision(vec![])],
        day(2026, 11, 9, 9),
    );
    assert_eq!(outcome.context().input, "what is the weather?");
    assert!(outcome.context().goal.is_none());
    let restarted = state.reopen();
    assert!(State::load(&restarted).expect("state").goal().is_none());
    assert!(eja::adapters::render_context(&outcome.context()).contains("CURRENT GOAL:\n  (none)"));
}

#[test]
fn unrelated_input_preserves_a_confirmed_goal() {
    let state = support::State::new("continuity-goal");
    let store = state.store();
    propose_and_confirm(
        &store,
        ProposalKind::Goal,
        "ship the release",
        None,
        day(2026, 11, 9, 9),
    );
    let outcome = turn_at_time(
        &store,
        "what is the weather?",
        vec![decision(vec![])],
        day(2026, 11, 9, 10),
    );
    let context = outcome.context();
    let goal = context.goal.as_ref().expect("goal");
    assert_eq!(goal.text, "ship the release");
    assert!(eja::adapters::render_context(&outcome.context())
        .contains(&format!("#{} ship the release", goal.id)));
    assert_eq!(outcome.context().input, "what is the weather?");
}

#[test]
fn restart_and_model_switch_preserve_identity() {
    let state = support::State::new("continuity-identity");
    let store = state.store();
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "answer with the conclusion first",
        None,
        day(2026, 11, 9, 9),
    );

    let store = state.reopen();
    turn_at_time(
        &store,
        "what did I commit to?",
        vec![decision(vec![])],
        day(2026, 11, 10, 9),
    );

    let fold = State::load(&store).expect("state");
    let context = fold.project("what did I commit to?", None, day(2026, 11, 10, 10));
    assert_eq!(context.commitments.len(), 1);
    assert_eq!(
        context.commitments[0].text,
        "answer with the conclusion first"
    );
}

#[test]
fn correction_by_id_survives_restart() {
    let state = support::State::new("continuity-correction");
    let store = state.store();
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "answer with the conclusion first",
        None,
        day(2026, 11, 9, 9),
    );
    let commitment = State::load(&store).expect("state").commitments()[0].clone();

    propose_and_confirm(
        &store,
        ProposalKind::Correction,
        "drop that commitment",
        Some(commitment.id),
        day(2026, 11, 9, 10),
    );

    let store = state.reopen();
    let events = store.events().expect("events");
    assert!(State::fold(events.clone()).commitments().is_empty());
    assert!(events
        .iter()
        .any(|event| event.kind == Kind::Correction && event.closes == Some(commitment.id)));
}

#[test]
fn correction_can_close_a_goal_without_language_matching() {
    let state = support::State::new("continuity-withdraw");
    let store = state.store();
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
    assert!(State::fold(events).goal().is_none());
}

#[test]
fn replacements_preserve_their_kind_and_revision_chain_across_restart() {
    for kind in [ProposalKind::Goal, ProposalKind::Commitment] {
        let state = support::State::new("continuity-replace");
        let store = state.store();
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
        let restarted = state.reopen();
        let fold = State::load(&restarted).expect("state");
        let context = fold.project("unrelated input", None, day(2026, 11, 10, 9));
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
        let events = state.events();
        assert!(State::fold(events.clone()).goal().is_none());
        assert!(State::fold(events.clone()).commitments().is_empty());
        assert_eq!(events.iter().filter(|e| e.kind == event_kind).count(), 3);
    }
}

#[test]
fn utc_time_is_one_source_for_turn_task_and_worker() {
    let state = support::State::new("continuity-clock");
    let store = state.store();
    let at = day(2026, 11, 9, 12);
    let outcome = turn_at_time(
        &store,
        "make a checklist",
        vec![
            decision(vec![aja("release")]),
            decision(vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "ok".to_string(),
            }]),
        ],
        at,
    );

    assert_eq!(outcome.context().now, at.timestamp());
    let task = &outcome.context().tasks[0];
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.created_at, at.timestamp());
    assert_eq!(
        task.run.as_ref().and_then(|run| run.digest_at),
        Some(at.timestamp())
    );
    assert_eq!(task.closed_at, Some(at.timestamp()));

    let events = store.events().expect("events");
    let assignment = events
        .iter()
        .find(|event| event.kind == Kind::TaskAssigned)
        .and_then(|event| event.data.as_ref())
        .and_then(|data| Assignment::deserialize(data).ok())
        .expect("assignment");
    assert_eq!(assignment.issued_at, at.timestamp());
}

#[test]
fn temporal_gap_is_numeric_and_language_free() {
    let state = support::State::new("continuity-gap");
    let store = state.store();
    propose_and_confirm(
        &store,
        ProposalKind::Commitment,
        "deliver Wednesday",
        None,
        day(2026, 11, 9, 9),
    );

    let later = day(2026, 11, 12, 18);
    let fold = State::load(&store).expect("state");
    let context = fold.project("what is due?", None, later);
    assert_eq!(context.now, later.timestamp());
    assert_eq!(context.commitments[0].at, day(2026, 11, 9, 9).timestamp());
}
