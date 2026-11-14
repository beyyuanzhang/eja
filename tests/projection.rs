use eja::adapters::{render_context, render_context_within};
use eja::{project, EventMeta, Kind, Store};

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
    let context = project(&store, "check", None, chrono::Utc::now()).expect("project");
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

    // A budgeted view states its landing scale and stays inside the budget.
    let tight = render_context_within(&context, 400);
    assert!(tight.len() <= 400, "{}", tight.len());
    assert!(tight.contains("[view "), "{tight}");
    assert!(tight.contains("unfold by handle"), "{tight}");
}

#[test]
fn a_record_that_does_not_fit_is_cut_at_the_budget_and_says_so() {
    use eja::{LookSource, Observation, Selector};
    let dir = std::env::temp_dir().join(format!("eja-budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let evidence = vec![Observation {
        source: LookSource::Node,
        target: "big.json".to_string(),
        arguments: serde_json::Value::Null,
        selector: Selector::Whole,
        result: serde_json::json!({"text": "a".repeat(5000)}),
        error: None,
    }];
    let context = eja::project_with_evidence(&store, "go", None, chrono::Utc::now(), &evidence)
        .expect("project");
    let view = render_context_within(&context, 400);
    assert!(view.contains("[view "), "{view}");
    assert!(view.contains("unfold by handle"), "{view}");
    assert!(view.len() <= 400, "{}", view.len());
}
