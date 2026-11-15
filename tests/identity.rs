mod support;
use eja::adapters::{render_context, render_context_within, Ports, SYSTEM_PROMPT};
use eja::{project, EventMeta, Host, Kind, Selector, Store};
use support::control;

/// The contract states the protocol; the projection states the history. Neither
/// states who the subject is.
#[test]
fn the_contract_is_protocol_not_character() {
    assert!(SYSTEM_PROMPT.contains("Return ONE JSON object"));
    assert!(SYSTEM_PROMPT.contains("\"op\":\"dispatch\""));
    assert!(!SYSTEM_PROMPT.contains("You are"), "{SYSTEM_PROMPT}");
    assert!(
        !SYSTEM_PROMPT.to_lowercase().contains("personality"),
        "{SYSTEM_PROMPT}"
    );
}

#[test]
fn the_self_survives_budget_pressure_before_the_indexes_do() {
    let dir = std::env::temp_dir().join(format!("eja-identity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    store
        .append_simple(Kind::Goal, "ship the release", 1, 1)
        .unwrap();
    store
        .append_simple(Kind::RecordDecision, "answer the question first", 1, 2)
        .unwrap();
    for index in 0..80 {
        store
            .append(
                Kind::TaskOpened,
                &format!("task:{index}"),
                &format!("task {index}"),
                1,
                EventMeta::data(
                    1,
                    serde_json::json!({"kind":"aja","title":format!("task {index}"),
                        "request":"r","origin":"o","acceptance":"a"}),
                ),
            )
            .unwrap();
    }
    let context = project(&store, "go", None, eja::now_utc()).expect("project");

    // Reading order is a claim about what the subject is.
    let whole = render_context(&context);
    let goal = whole.find("CURRENT GOAL").expect("goal");
    let identity = whole.find("ACTIVE IDENTITY").expect("identity");
    let index = whole.find("TASK INDEX").expect("index");
    assert!(goal < identity && identity < index, "{whole}");

    // Tight, the index folds to an address and the self stays whole.
    let tight = render_context_within(&context, 900);
    assert!(tight.len() <= 900, "{}", tight.len());
    assert!(tight.contains("ship the release"), "{tight}");
    assert!(tight.contains("answer the question first"), "{tight}");
    assert!(!tight.contains("task 79"), "{tight}");
}

#[test]
fn a_folded_region_is_still_read_by_its_address() {
    let dir = std::env::temp_dir().join(format!("eja-address-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let mut ids = Vec::new();
    for index in 0..40 {
        let event = store
            .append_simple(
                Kind::RecordDecision,
                &format!("decision {index}"),
                1,
                index as i64,
            )
            .unwrap();
        ids.push(event.id);
    }
    let events = store.events().unwrap();
    let ports = Ports::default();
    for id in ids {
        let value = ports
            .look(
                &format!("log:event:{id}"),
                &serde_json::Value::Null,
                &Selector::Whole,
                &events,
                &control(),
            )
            .expect("addressable");
        assert_eq!(value["events"][0]["id"], serde_json::json!(id));
        assert_eq!(value["events"][0]["kind"], serde_json::json!("decision"));
    }
}

#[test]
fn addresses_are_one_scheme_and_plain_targets_are_host_tools() {
    assert!(eja::Address::parse("log:event:1").is_some());
    assert!(eja::Address::parse("record:3/4").is_some());
    assert!(eja::Address::parse("record:3/report.md").is_some());
    assert!(eja::Address::parse("world:README.md").is_some());
    assert!(eja::Address::parse("history").is_none());
    assert!(eja::Address::parse("read_file").is_none());
    assert!(eja::Address::parse("log:").is_none());
}
#[test]
fn an_address_without_its_authority_is_an_error_not_a_fallback() {
    let events = Vec::new();
    let ports = Ports::default();
    let look = |target: &str, selector: Selector| {
        ports.look(
            target,
            &serde_json::Value::Null,
            &selector,
            &events,
            &control(),
        )
    };
    // No record reader, no capabilities: each authority refuses instead of
    // quietly reading something else.
    assert!(look("record:1/report.md", Selector::Whole).is_err());
    assert!(look("world:README.md", Selector::Whole).is_err());
    // A world address names one path; it has no page to select.
    assert!(look("world:README.md", Selector::Range { from: 0, lines: 1 }).is_err());
    // An unknown target is a host tool, so with no capabilities it is refused too.
    assert!(look("read_file", Selector::Whole).is_err());
}
