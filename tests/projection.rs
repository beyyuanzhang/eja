use eja::adapters::{render_context, render_context_within};
use eja::{EventMeta, Kind, Observation, ObservationKind, Selector, State, Store};

#[test]
fn projection_keeps_every_recorded_commitment_and_says_so() {
    let dir = std::env::temp_dir().join(format!("eja-projection-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    for index in 0..210 {
        store
            .append(
                Kind::Commitment,
                "eja",
                &format!("commitment {index}"),
                1,
                EventMeta::new(index as i64),
            )
            .expect("append");
    }
    let context = State::load(&store)
        .expect("state")
        .project("check", None, chrono::Utc::now());
    assert_eq!(context.commitments.len(), 210);
    assert_eq!(store.events().expect("events").len(), 210);

    // Nothing is silently cut: an unbudgeted view shows everything whole.
    let full = render_context(&context);
    assert!(full.contains("OPEN COMMITMENTS (210):"), "{full}");
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
    let dir = std::env::temp_dir().join(format!("eja-budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    // The only way an observation exists is as a committed fact. A projection
    // reads it back from the journal like every other fact.
    let observation = Observation {
        kind: ObservationKind::Look,
        target: "record:1/big.json".to_string(),
        arguments: serde_json::Value::Null,
        selector: Selector::Whole,
        result: serde_json::json!({"text": "a".repeat(5000)}),
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
    let context = State::load(&store)
        .expect("state")
        .project("go", None, chrono::Utc::now());
    assert_eq!(context.evidence.len(), 1);
    assert_eq!(context.evidence[0], observation);
    assert!(
        render_context(&context).contains("record:1/big.json"),
        "the projection shows what the log recorded"
    );
    let view = render_context_within(&context, 400);
    assert!(view.contains("[view "), "{view}");
    assert!(view.len() <= 400, "{}", view.len());
}
