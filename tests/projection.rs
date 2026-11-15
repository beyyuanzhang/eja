mod support;

use eja::adapters::{render_context, render_context_within};
use eja::{EventMeta, Kind, Observation, ObservationKind, Selector, State};
use support::State as Fixture;

#[test]
fn projection_keeps_every_recorded_commitment_and_says_so() {
    let fixture = Fixture::new("projection");
    let store = fixture.store();
    for index in 0..210 {
        store
            .append(
                Kind::Commitment,
                "eja",
                &format!("commitment {index}"),
                1,
                EventMeta::new(i64::from(index)),
            )
            .expect("append");
    }
    let state = State::load(&store).expect("state");
    let context = state.project("check", None, chrono::Utc::now());
    assert_eq!(context.commitments.len(), 210);
    assert_eq!(store.events().expect("events").len(), 210);

    // Nothing is silently cut: an unbudgeted view shows everything whole.
    let full = render_context(&context);
    assert!(full.contains("COMMITMENTS (210):"), "{full}");
    assert_eq!(
        full.matches("commitment ").count(),
        210,
        "every recorded commitment is rendered"
    );
    assert!(!full.contains("[view "), "a whole view needs no notice");

    // A budgeted view states its landing scale and stays inside the budget. Every
    // folded section still names the addresses that unfold it.
    let tight = render_context_within(&context, 400);
    assert!(tight.len() <= 400, "{}", tight.len());
    assert!(tight.contains("[view "), "{tight}");
    assert!(tight.contains("unfold by address"), "{tight}");
    assert!(tight.contains("log:event:"), "{tight}");
}

#[test]
fn evidence_is_folded_from_the_journal_never_from_the_caller() {
    let fixture = Fixture::new("budget");
    let store = fixture.store();
    // The only way an observation exists is as a committed fact. A projection
    // reads it back from the journal like every other fact.
    let result = serde_json::json!({"text": "a".repeat(5000)});
    let observation = Observation {
        kind: ObservationKind::Look,
        target: "record:1/big.json",
        arguments: &serde_json::Value::Null,
        selector: Selector::Whole,
        result: &result,
        error: None,
    };
    store
        .append(
            Kind::Observed,
            "eja",
            "",
            1,
            EventMeta::data(1, serde_json::to_value(&observation).expect("observation")),
        )
        .expect("append");
    let state = State::load(&store).expect("state");
    let context = state.project("go", None, chrono::Utc::now());
    assert_eq!(context.evidence.len(), 1);
    assert!(context.evidence[0].same_content(&observation));
    assert!(
        render_context(&context).contains("record:1/big.json"),
        "the projection shows what the log recorded"
    );
    let view = render_context_within(&context, 400);
    assert!(view.contains("[view "), "{view}");
    assert!(view.len() <= 400, "{}", view.len());
}
