mod support;

use eja::adapters::Ports;
use eja::{Action, Decision, Kind, ProposalKind, State, Store, TurnOptions, TurnOutcome};
use support::{decision, turn_at, Done, Script, State as Fixture};

/// The one thing the kit does not cover: a turn the host authorizes in advance.
fn approved(
    store: &Store,
    input: &'static str,
    decisions: Vec<Decision>,
    approvals: Vec<u64>,
) -> TurnOutcome<'static> {
    let brain = Script::new(decisions);
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
fn proposal_does_not_change_identity_until_confirmed() {
    let fixture = Fixture::new("proposals");
    let store = fixture.store();
    let proposed = turn_at(
        &store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "answer first".to_string(),
        }])],
    );
    assert_eq!(proposed.context().proposals.len(), 1);
    assert!(State::load(&store).expect("state").commitments().is_empty());

    let proposal_id = proposed.context().proposals[0].id;
    let confirmed = approved(
        &store,
        "confirm",
        vec![decision(vec![Action::Confirm { proposal_id }])],
        vec![proposal_id],
    );
    assert!(confirmed.context().proposals.is_empty());
    assert_eq!(State::load(&store).expect("state").commitments().len(), 1);
}

#[test]
fn structured_approval_does_not_invent_user_words() {
    let fixture = Fixture::new("proposals-approval-without-words");
    let store = fixture.store();
    let proposed = turn_at(
        &store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "answer first".to_string(),
        }])],
    );
    let proposal_id = proposed.context().proposals[0].id;

    let confirmed = approved(
        &store,
        "",
        vec![decision(vec![Action::Confirm { proposal_id }])],
        vec![proposal_id],
    );

    assert!(confirmed.context().proposals.is_empty());
    let events = store.events().expect("events");
    assert!(events.iter().any(|event| event.kind == Kind::Approval));
    assert!(
        events
            .iter()
            .all(|event| !(event.kind == Kind::User && event.text.is_empty())),
        "a host gesture must not create a fabricated user message"
    );
}

#[test]
fn a_situation_opens_a_turn_and_silence_is_whole() {
    let fixture = Fixture::new("situation-silence");
    let store = fixture.store();
    turn_at(
        &store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "answer first".to_string(),
        }])],
    );

    for (gesture, kind, input) in [
        (Kind::Returned, Kind::Returned, "user returned"),
        (Kind::Departed, Kind::Departed, "user departed"),
        (Kind::Alone, Kind::Alone, "alone; time passes"),
    ] {
        let brain = Script::new(vec![Decision {
            reply: String::new(),
            actions: Vec::new(),
            refused: Vec::new(),
        }]);
        let outcome = eja::turn_at_with_options(
            &store,
            "",
            &brain,
            &Done::default(),
            &Ports::default(),
            eja::now_utc(),
            &TurnOptions {
                gesture: Some(gesture),
                ..Default::default()
            },
        )
        .expect("situation turn");
        assert_eq!(outcome.reply, "", "silence is a whole answer");
        assert_eq!(outcome.context().input, input);
        let events = store.events().expect("events");
        let fact = events
            .iter()
            .find(|event| event.kind == kind)
            .expect("situation fact recorded");
        assert!(
            events
                .iter()
                .all(|event| event.turn != fact.turn || event.kind != Kind::User),
            "a situation must not fabricate user words"
        );
    }
    assert!(State::load(&store).expect("state").commitments().is_empty());
}

#[test]
fn a_situation_carries_the_users_words_without_fabricating_them() {
    let fixture = Fixture::new("situation-with-words");
    let store = fixture.store();
    let brain = Script::new(vec![decision(vec![])]);
    eja::turn_at_with_options(
        &store,
        "hello",
        &brain,
        &Done::default(),
        &Ports::default(),
        eja::now_utc(),
        &TurnOptions {
            gesture: Some(Kind::Returned),
            ..Default::default()
        },
    )
    .expect("situation with words");
    let events = store.events().expect("events");
    assert!(
        events.iter().all(|event| event.kind != Kind::User),
        "the caller's words beside a situation are not the user's speech"
    );
    assert!(events.iter().any(|event| event.kind == Kind::Returned));
}

#[test]
fn a_proposal_cannot_be_confirmed_in_the_same_turn() {
    let fixture = Fixture::new("proposals-same-turn");
    let store = fixture.store();
    turn_at(
        &store,
        "do both",
        vec![decision(vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "answer first".to_string(),
            },
            Action::Confirm { proposal_id: 2 },
        ])],
    );
    assert!(State::load(&store).expect("state").commitments().is_empty());
}

#[test]
fn rejection_leaves_identity_unchanged() {
    let fixture = Fixture::new("proposals-reject");
    let store = fixture.store();
    let proposed = turn_at(
        &store,
        "propose",
        vec![decision(vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "answer first".to_string(),
        }])],
    );
    let proposal_id = proposed.context().proposals[0].id;
    let rejected = turn_at(
        &store,
        "reject",
        vec![decision(vec![Action::Reject {
            proposal_id,
            note: "no".to_string(),
        }])],
    );
    assert!(rejected.context().proposals.is_empty());
    assert!(State::load(&store).expect("state").commitments().is_empty());
}

#[test]
fn replacements_wait_for_confirmation_and_revalidate_the_target() {
    let fixture = Fixture::new("proposals-replace");
    let store = fixture.store();
    let old = store
        .append_simple(Kind::Commitment, "original", 1, 1)
        .unwrap();
    let out = turn_at(
        &store,
        "propose",
        vec![decision(vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: Some(old.id),
                text: "replacement".into(),
            },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: Some(old.id),
                text: "stale replacement".into(),
            },
        ])],
    );
    assert_eq!(out.context().proposals.len(), 2);
    assert_eq!(
        State::load(&store).expect("state").commitments()[0].id,
        old.id
    );
    let first = out.context().proposals[0].id;
    let stale = out.context().proposals[1].id;
    let out = approved(
        &store,
        "confirm",
        vec![decision(vec![
            Action::Confirm { proposal_id: first },
            Action::Confirm { proposal_id: stale },
        ])],
        vec![first, stale],
    );
    assert_eq!(out.context().commitments.len(), 1);
    assert_eq!(out.context().commitments[0].text, "replacement");
    assert_eq!(out.context().commitments[0].closes, Some(old.id));
    assert_eq!(out.context().proposals.len(), 1);
    assert_eq!(out.context().proposals[0].id, stale);
    let events = store.events().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == Kind::Commitment
                && e.data
                    .as_ref()
                    .is_some_and(|data| data.get("proposal_id").is_some()))
            .count(),
        1
    );
}

#[test]
fn invalid_revision_targets_do_not_create_proposals() {
    let fixture = Fixture::new("proposals-invalid");
    let store = fixture.store();
    let old = store
        .append_simple(Kind::Commitment, "original", 1, 1)
        .unwrap();
    let out = turn_at(
        &store,
        "propose",
        vec![decision(vec![
            Action::Propose {
                kind: ProposalKind::Goal,
                target_id: Some(old.id),
                text: "wrong kind".into(),
            },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: Some(999),
                text: "missing target".into(),
            },
            Action::Propose {
                kind: ProposalKind::Correction,
                target_id: None,
                text: "missing target".into(),
            },
        ])],
    );
    assert!(out.context().proposals.is_empty());
    assert!(out.context().goal.is_none());
    assert_eq!(out.context().commitments[0].id, old.id);
}
