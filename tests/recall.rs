mod support;

use eja::adapters::Ports;
use eja::{Action, Decision, Kind, ProposalKind, Selector, State, Store, TurnOptions, TurnOutcome};
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
fn recall_is_an_id_result_not_text_matching() {
    let fixture = Fixture::new("recall");
    let store = fixture.store();
    let proposed = turn_at(
        &store,
        "commit",
        vec![decision(vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "arbitrary old fact".to_string(),
        }])],
    );
    let proposal_id = proposed.context().proposals.last().expect("proposal").id;
    approved(
        &store,
        "confirm",
        vec![decision(vec![Action::Confirm { proposal_id }])],
        vec![proposal_id],
    );
    let commitment = State::load(&store).expect("state").commitments()[0].clone();

    let outcome = turn_at(
        &store,
        "what did we decide?",
        vec![
            decision(vec![Action::Look {
                target: format!("log:event:{}", commitment.id),
                arguments: serde_json::Value::Null,
                selector: Selector::Whole,
            }]),
            decision(vec![]),
        ],
    );

    assert_eq!(outcome.context().evidence.len(), 1);
    let events = outcome.context().evidence[0].result["events"]
        .as_array()
        .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], commitment.id);
    assert_eq!(events[0]["kind"], "commitment");
    assert_eq!(outcome.context().evidence.len(), 1);
    assert!(store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::Observed));
}
