use eja::{project, EventMeta, Kind, Store, MAX_COMMITMENTS};

#[test]
fn projection_bounds_open_commitments_without_compacting_the_log() {
    let dir = std::env::temp_dir().join(format!("eja-projection-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    for index in 0..(MAX_COMMITMENTS + 10) {
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
    assert_eq!(context.commitments.len(), MAX_COMMITMENTS);
    assert_eq!(store.events().expect("events").len(), MAX_COMMITMENTS + 10);
}
