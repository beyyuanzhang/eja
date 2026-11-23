use eja_core::{Eja, Error, Fact, Outcome, Source, Timestamp, FACT_BYTES};
use serde_json::json;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[test]
fn migrates_v3_subject_db_to_v4_preserving_concerns_and_actions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let db = rusqlite::Connection::open(&path)?;
    db.execute_batch(
        "CREATE TABLE facts (
             id INTEGER PRIMARY KEY,
             at INTEGER NOT NULL,
             body TEXT NOT NULL,
             concern INTEGER,
             live INTEGER
         );
         INSERT INTO facts(id, at, body, concern, live) VALUES
             (1, 100, '{\"kind\":\"kept\",\"concern\":1,\"text\":\"watch\"}', 1, 1),
             (2, 101, '{\"kind\":\"acted\",\"capability\":\"shell\",\"input\":null}', NULL, NULL),
             (3, 102, '{\"kind\":\"returned\",\"action\":2,\"outcome\":{\"status\":\"observed\",\"detail\":\"ok\"}}', NULL, NULL),
             (4, 103, '{\"kind\":\"acted\",\"capability\":\"shell\",\"input\":null}', NULL, NULL),
             (5, 104, '{\"kind\":\"said\",\"text\":\"hi\",\"in_response_to\":\"person\"}', NULL, NULL);
         PRAGMA user_version = 3;",
    )?;
    drop(db);

    let reopened = Eja::open(&path)?;
    assert_eq!(reopened.concerns(reopened.revision()?)?.len(), 1);
    assert_eq!(reopened.unfinished_actions()?, vec![4]);
    assert_eq!(reopened.evidence(2)?.latest.unwrap().id, 3);
    assert!(matches!(reopened.read(5)?.fact, Fact::Said { ref text } if text == "hi"));
    reopened.set_meta("host.anchor", "5")?;
    assert_eq!(reopened.get_meta("host.anchor")?.as_deref(), Some("5"));
    Ok(())
}

#[test]
fn concerns_revisions_releases_and_evidence_preserve_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let eja = Eja::open(&path)?;
    eja.notice(Source::Person, "评审让我有点累")?;
    let initial = eja.keep(None, "也许想讨论评审")?;
    let concern = initial.id;
    assert!(matches!(eja.keep(None, "   "), Err(Error::Invalid(_))));
    assert!(matches!(eja.say("   "), Err(Error::Invalid(_))));
    let said = eja.say("我记住了")?;
    assert!(matches!(eja.read(said)?.fact, Fact::Said { ref text, .. } if text == "我记住了"));
    drop(eja);

    let eja = Eja::open(&path)?;
    let correction = eja.notice(Source::Person, "下次听我说完就好")?;
    let revised = eja.keep(Some(concern), "先听完")?;
    let tick = eja.notice(Source::Clock, "第二天")?;
    let concerns = eja.concerns(eja.revision()?)?;
    assert_eq!(concerns.len(), 1);
    assert_eq!(concerns[0].id, concern);
    assert_eq!(concerns[0].latest, revised.id);
    assert_eq!(concerns[0].text, "先听完");
    assert!(
        matches!(eja.read(concern)?.fact, Fact::Kept { concern: id, ref text } if id == concern && text == "也许想讨论评审")
    );
    let ev = eja.evidence(concern)?;
    assert_eq!(ev.entry.id, concern);
    assert_eq!(ev.latest.as_ref().unwrap().id, revised.id);
    assert!(matches!(
        eja.read(correction)?.fact,
        Fact::Noticed {
            source: Source::Person,
            ..
        }
    ));

    let released = eja.release(concern)?;
    assert!(eja.concerns(eja.revision()?)?.is_empty());
    assert_eq!(
        eja.evidence(concern)?.latest.as_ref().unwrap().id,
        released.id
    );
    assert!(matches!(eja.release(concern), Err(Error::Invalid(_))));
    assert!(matches!(
        eja.keep(Some(concern), "再次修改"),
        Err(Error::Invalid(_))
    ));
    assert_eq!(eja.concerns(tick)?[0].text, "先听完");
    Ok(())
}

#[test]
fn tx_evidence_reflects_in_tx_settlement() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let act = eja.act(eja.revision()?, "shell", json!({"command": "check"}))?;
    let unk = eja.finish(act.id, Outcome::Unknown("timeout".into()))?;
    eja.transaction(|tx| {
        assert!(tx.evidence(unk.id)?.latest.is_none());
        let obs = tx.finish(act.id, Outcome::Observed("green".into()))?;
        let ev = tx.evidence(unk.id)?;
        assert_eq!(ev.latest.as_ref().map(|e| e.id), Some(obs.id));
        Ok(())
    })?;
    Ok(())
}

#[test]
fn unsettled_unknowns_tracks_only_unresolved_unknown_receipts_at_revision() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let a1 = eja.act(eja.revision()?, "shell", json!({"command": "a1"}))?;
    let u1 = eja.finish(a1.id, Outcome::Unknown("timeout 1".into()))?;
    let a2 = eja.act(eja.revision()?, "shell", json!({"command": "a2"}))?;
    let u2 = eja.finish(a2.id, Outcome::Unknown("disconnect 2".into()))?;
    let rev_before_settle = eja.revision()?;
    assert_eq!(
        eja.unsettled_unknowns(rev_before_settle, 10)?
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        vec![u1.id, u2.id]
    );
    eja.finish(a1.id, Outcome::Observed("done".into()))?;
    let rev_after_settle = eja.revision()?;
    assert_eq!(
        eja.unsettled_unknowns(rev_after_settle, 10)?
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        vec![u2.id]
    );
    assert_eq!(eja.unsettled_unknowns(rev_before_settle, 10)?.len(), 2);
    Ok(())
}

#[test]
fn only_a_fresh_granted_act_can_issue_a_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let first = Eja::open(&path)?;
    let second = Eja::open(&path)?;
    let basis = first.notice(Source::World, "an observation")?;
    assert!(matches!(
        first.act(basis, "", json!({"x": 1})),
        Err(Error::Invalid(_))
    ));
    let rev = second.notice(Source::Person, "one more thing")?;
    assert!(matches!(
        first.act(basis, "new.tool", json!({"x": 1})),
        Err(Error::Conflict)
    ));
    let call = first.act(rev, "new.tool", json!({"x": 1}))?;
    assert_eq!(call.input, json!({"x":1}));
    assert!(matches!(
        second.act(rev, "new.tool", json!({"x": 1})),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        second.finish(basis, Outcome::Observed("wrong target".into())),
        Err(Error::Invalid(_))
    ));
    // Simulate the host disappearing after dispatch and before observing the result.
    drop(first);
    drop(second);
    let resumed = Eja::open(&path)?;
    assert_eq!(resumed.revision()?, call.id);
    let unknown = || Outcome::Unknown("host stopped".into());
    let receipt = resumed.finish(call.id, unknown())?;
    assert_eq!(resumed.finish(call.id, unknown())?, receipt);
    let settled = resumed.finish(call.id, Outcome::Observed("verified later".into()))?;
    assert!(settled.id > receipt.id);
    assert_eq!(resumed.finish(call.id, unknown())?, receipt);
    assert_eq!(
        resumed.finish(call.id, Outcome::Observed("verified later".into()))?,
        settled
    );
    assert!(matches!(
        resumed.finish(call.id, Outcome::Failed("contradiction".into())),
        Err(Error::Invalid(_))
    ));
    assert_eq!(resumed.revision()?, settled.id);
    Ok(())
}

#[test]
fn range_queries_and_byte_budget_enforce_bounds() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let first = eja.notice(Source::Person, "initial")?;
    for n in 0..12 {
        eja.keep(None, n.to_string())?;
    }
    let head = eja.revision()?;
    assert_eq!(eja.concerns(head)?.len(), 12);
    let all = eja.entries_after(0, head)?;
    assert_eq!(all.len(), 13);
    assert_eq!(all.first().unwrap().id, first);
    assert_eq!(all.last().unwrap().id, head);
    let tail = eja.recent(5)?;
    assert_eq!(tail.len(), 5);
    assert_eq!(tail.first().unwrap().id, head - 4);
    assert_eq!(tail.last().unwrap().id, head);
    assert!(eja.notice(Source::World, "字".repeat(FACT_BYTES)).is_err());
    assert_eq!(eja.revision()?, head);
    Ok(())
}

#[test]
fn simultaneous_acts_issue_only_one_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let first = Eja::open(&path)?;
    let second = Eja::open(&path)?;
    let revision = first.notice(Source::Clock, "wake")?;
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let run = |eja: Eja| {
            barrier.wait();
            match eja.act(revision, "one", json!(null)) {
                Ok(_) => true,
                Err(Error::Conflict) => false,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        let a = scope.spawn(move || run(first));
        let b = scope.spawn(move || run(second));
        assert_ne!(a.join().unwrap(), b.join().unwrap());
    });
    assert_eq!(Eja::open(&path)?.revision()?, revision + 1);
    Ok(())
}

#[test]
fn oversized_receipt_truncates_to_fact_budget_and_stays_idempotent() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    assert_eq!(eja.revision()?, 0);
    let basis = eja.notice(Source::Person, "test")?;
    let call = eja.act(basis, "read", json!(null))?;
    let detail = "雨\0\"".repeat(FACT_BYTES);
    let receipt = eja.finish(call.id, Outcome::Observed(detail.clone()))?;
    assert!(serde_json::to_vec(&receipt.fact)?.len() <= FACT_BYTES);
    assert!(
        matches!(receipt.fact, Fact::Returned { outcome: Outcome::Observed(ref s), .. } if s.contains("truncated") && s.contains("sha256:"))
    );
    assert_eq!(
        eja.finish(call.id, Outcome::Observed(detail.clone()))?,
        receipt
    );
    assert!(eja
        .finish(call.id, Outcome::Observed(detail + "!"))
        .is_err());
    Ok(())
}

#[test]
fn an_unknown_receipt_can_be_settled_only_once_by_competing_hosts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("subject.db");
    let seed = Eja::open(&db)?;
    let notice = seed.notice(Source::World, "tick")?;
    let call = seed.act(notice, "read", json!(null))?;
    let unknown = seed.finish(call.id, Outcome::Unknown("lost".into()))?;
    drop(seed);
    let a = Eja::open(&db)?;
    let b = Eja::open(&db)?;
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        let settle = |host: Eja, outcome| {
            barrier.wait();
            host.finish(call.id, outcome).is_ok()
        };
        let left = s.spawn(move || settle(a, Outcome::Observed("yes".into())));
        let right = s.spawn(move || settle(b, Outcome::Failed("no".into())));
        assert_ne!(left.join().unwrap(), right.join().unwrap());
    });
    let e = Eja::open(db)?;
    assert_eq!(e.finish(call.id, Outcome::Unknown("lost".into()))?, unknown);
    assert_eq!(e.revision()?, unknown.id + 1);
    Ok(())
}

#[test]
fn physical_time_is_rfc3339_monotonic_and_visible_across_reopen() -> Result<()> {
    for sample in [
        "1970-01-01T00:00:00Z",
        "2000-02-29T12:34:56Z",
        "2024-02-29T23:59:59Z",
        "2026-11-20T11:20:39Z",
    ] {
        let parsed = Timestamp::parse_rfc3339(sample)?;
        assert_eq!(parsed.to_string(), sample);
        let encoded = serde_json::to_string(&parsed)?;
        assert_eq!(encoded, format!("\"{sample}\""));
        assert_eq!(serde_json::from_str::<Timestamp>(&encoded)?, parsed);
    }
    assert!(Timestamp::parse_rfc3339("2026-02-29T00:00:00Z").is_err());
    assert!(Timestamp::parse_rfc3339("2026-13-01T00:00:00Z").is_err());

    let t0 = Timestamp::parse_rfc3339("2026-11-01T09:00:00Z")?;
    let clock = Arc::new(AtomicI64::new(t0.as_unix()));
    let make_clock = |clock: &Arc<AtomicI64>| {
        let shared = Arc::clone(clock);
        move || Timestamp::from_unix(shared.load(Ordering::SeqCst))
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let eja = Eja::open(&path)?.with_clock(make_clock(&clock));
    let first = eja.notice(Source::Person, "feeling tired")?;
    assert_eq!(eja.read(first)?.at, t0);

    // Backward wall-clock jump is clamped so physical time never contradicts causal order.
    clock.store(t0.as_unix() - 3600, Ordering::SeqCst);
    let second = eja.notice(Source::World, "skewed tick")?;
    assert_eq!(eja.read(second)?.at, t0);

    let t1 = Timestamp::parse_rfc3339("2026-11-02T15:30:00Z")?;
    clock.store(t1.as_unix(), Ordering::SeqCst);
    eja.keep(None, "check tomorrow")?;
    assert_eq!(eja.now()?, t1);
    assert_eq!(eja.concerns(eja.revision()?)?[0].at, t1);
    drop(eja);

    // Reopen three weeks later with no new write: now() still reflects elapsed physical time.
    let t2 = Timestamp::parse_rfc3339("2026-11-23T09:00:00Z")?;
    clock.store(t2.as_unix(), Ordering::SeqCst);
    let reopened = Eja::open(&path)?.with_clock(make_clock(&clock));
    assert_eq!(reopened.now()?, t2);
    assert_eq!(reopened.concerns(reopened.revision()?)?[0].at, t1);
    assert_eq!(reopened.read(first)?.at, t0);
    Ok(())
}

#[test]
fn subject_db_sets_schema_version_and_rejects_future_versions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let eja = Eja::open(&path)?;
    drop(eja);

    let conn = rusqlite::Connection::open(&path)?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    assert_eq!(version, 4);

    conn.execute_batch("PRAGMA user_version = 99;")?;
    drop(conn);

    assert!(Eja::open(&path).is_err());
    Ok(())
}

#[test]
fn unfinished_actions_finds_only_unsettled_actions_on_long_journal() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    for index in 0..300 {
        let rev = eja.revision()?;
        let call = eja.act(rev, "shell", json!({ "n": index }))?;
        eja.finish(call.id, Outcome::Observed("ok".into()))?;
    }
    let rev = eja.revision()?;
    let pending = eja.act(rev, "shell", json!({ "n": 300 }))?;

    assert_eq!(eja.unfinished_actions()?, vec![pending.id]);
    Ok(())
}
