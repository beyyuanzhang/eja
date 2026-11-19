//! The one turn-input contract shared by the CLI, HTTP and kernel.
mod support;

use eja::{validate_turn_input, ChoiceSelection, Kind, TurnOptions};
use support::{Done, Script, State};

fn options() -> TurnOptions {
    TurnOptions::default()
}

#[test]
fn each_standalone_input_shape_is_valid() {
    assert!(validate_turn_input("hello", &options()).is_ok());

    let mut situation = options();
    situation.gesture = Some(Kind::Returned);
    assert!(validate_turn_input("", &situation).is_ok());

    let mut selection = options();
    selection.selection = Some(ChoiceSelection {
        question: 3,
        option: 0,
    });
    assert!(validate_turn_input("", &selection).is_ok());

    let mut approval = options();
    approval.approvals = vec![4];
    assert!(validate_turn_input("", &approval).is_ok());

    let mut denial = options();
    denial.denials = vec![5];
    assert!(validate_turn_input("", &denial).is_ok());
}

#[test]
fn mixed_or_ambiguous_inputs_are_refused_by_the_shared_rule() {
    let mut mixed = options();
    mixed.gesture = Some(Kind::Returned);
    assert!(validate_turn_input("words", &mixed).is_err());

    mixed.focus = Some(1);
    assert!(validate_turn_input("", &mixed).is_err());

    let mut gesture_approval = options();
    gesture_approval.gesture = Some(Kind::Departed);
    gesture_approval.approvals = vec![1];
    assert!(validate_turn_input("", &gesture_approval).is_err());

    let mut selected_text = options();
    selected_text.selection = Some(ChoiceSelection {
        question: 1,
        option: 0,
    });
    assert!(validate_turn_input("words", &selected_text).is_err());

    selected_text.denials = vec![2];
    assert!(validate_turn_input("", &selected_text).is_err());

    let mut contradictory = options();
    contradictory.approvals = vec![7];
    contradictory.denials = vec![7];
    assert!(validate_turn_input("", &contradictory).is_err());

    let mut zero = options();
    zero.approvals = vec![0];
    assert!(validate_turn_input("", &zero).is_err());
    zero.approvals.clear();
    zero.focus = Some(0);
    assert!(validate_turn_input("", &zero).is_err());

    let mut unknown = options();
    unknown.gesture = Some(Kind::Note);
    assert!(validate_turn_input("", &unknown).is_err());

    assert!(validate_turn_input(" \n ", &options()).is_err());
}

#[test]
fn an_invalid_mix_writes_no_business_fact() {
    let fixture = State::new("input-before-write");
    let store = fixture.store();
    let mut mixed = options();
    mixed.gesture = Some(Kind::Alone);
    let error = eja::turn_at_with_options(
        &store,
        "not alone",
        &Script::new(vec![]),
        &Done::default(),
        &eja::adapters::Ports::default(),
        eja::now_utc(),
        &mixed,
    )
    .expect_err("mixed input is refused");
    assert!(error.to_string().contains("own turn input"), "{error}");
    assert!(store.events().expect("events").is_empty());
}
