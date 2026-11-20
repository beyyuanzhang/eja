use eja_core::{Act, Eja, Error, Fact, Keep, Outcome, Source, Step, Stop, FACT_BYTES, PAGE};
use serde_json::json;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn keep(concern: Option<i64>, text: &str) -> Keep {
    Keep {
        concern,
        text: text.into(),
    }
}

#[test]
fn turn_feeds_read_evidence_and_stops_naturally_without_rest() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let first = eja.notice(Source::Person, "我喜欢沿河散步")?;
    eja.commit(first, &[keep(None, "周末的去处")], &[], &[], &[])?;
    let concern = eja.view()?.concerns[0].id;
    let change = eja.notice(Source::Person, "这阵子更想去山里")?;
    eja.commit(
        change,
        &[keep(Some(concern), "周末想去山里")],
        &[],
        &[],
        &[],
    )?;
    for _ in 0..PAGE {
        eja.notice(Source::Clock, "tick")?;
    }
    eja.notice(Source::Person, "明天想出去透口气")?;
    let mut steps = 0;
    let stop = eja.turn(
        &[],
        3,
        |view| {
            steps += 1;
            assert!(!view.recent.iter().any(|entry| entry.id == change));
            if steps == 1 {
                assert!(view.reads.is_empty());
                return Ok(Step {
                    read: vec![change, concern],
                    ..Step::default()
                });
            }
            assert_eq!(view.reads.len(), 2);
            assert_eq!(view.reads[0].entry.id, change);
            assert!(view.reads[0].latest.is_none());
            assert!(matches!(
                &view.reads[0].entry.fact,
                Fact::Noticed {
                    source: Source::Person,
                    text
                } if text == "这阵子更想去山里"
            ));
            assert_eq!(view.reads[1].entry.id, concern);
            assert_eq!(
                view.reads[1].latest.as_ref().unwrap().id,
                view.concerns[0].latest
            );
            Ok(Step {
                keep: vec![keep(Some(concern), "周末想去山里透口气")],
                ..Step::default()
            })
        },
        |_| unreachable!(),
    )?;
    assert_eq!(stop, Stop::Done);
    assert_eq!(steps, 2);
    let view = eja.view()?;
    assert!(view.reads.is_empty());
    assert_eq!(view.concerns[0].text, "周末想去山里透口气");
    Ok(())
}

#[test]
fn invalid_read_target_aborts_step_without_mutating_journal() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let notice = eja.notice(Source::Person, "在这里")?;
    assert!(matches!(
        eja.turn(
            &[],
            2,
            |_| Ok(Step {
                keep: vec![keep(None, "不应写入")],
                read: vec![notice + 1],
                ..Step::default()
            }),
            |_| unreachable!(),
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(eja.view()?.revision, notice);
    assert!(eja.view()?.concerns.is_empty());
    Ok(())
}

#[test]
fn batch_commit_is_atomic_and_rolls_back_on_partial_failure() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let start = eja.notice(Source::Person, "开始")?;
    let calls = eja.commit(
        start,
        &[keep(None, "第一件事"), keep(None, "第二件事")],
        &[],
        &[],
        &[],
    )?;
    assert!(calls.is_empty());
    let view = eja.view()?;
    assert_eq!(view.concerns.len(), 2);
    let first_concern = view.concerns[1].id;
    let second_concern = view.concerns[0].id;

    assert!(matches!(
        eja.commit(view.revision, &[keep(None, "回滚")], &[999], &[], &[],),
        Err(Error::Invalid(_))
    ));
    assert_eq!(eja.view()?.revision, view.revision);

    assert!(matches!(
        eja.commit(
            view.revision,
            &[],
            &[first_concern, first_concern],
            &[],
            &[],
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(eja.view()?.revision, view.revision);

    assert!(matches!(
        eja.commit(
            view.revision,
            &[keep(Some(first_concern), "已更新")],
            &[second_concern],
            &[Act {
                capability: "denied".into(),
                input: json!({}),
            }],
            &["allowed"],
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(eja.view()?.revision, view.revision);

    let calls = eja.commit(
        view.revision,
        &[keep(Some(first_concern), "已更新")],
        &[second_concern],
        &[Act {
            capability: "allowed".into(),
            input: json!({"ok": true}),
        }],
        &["allowed"],
    )?;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, "allowed");
    let updated = eja.view()?;
    assert_eq!(updated.concerns.len(), 1);
    assert_eq!(updated.concerns[0].id, first_concern);
    assert_eq!(updated.concerns[0].text, "已更新");
    Ok(())
}

#[test]
fn a_correction_changes_later_attention_and_preserves_both_sources() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let eja = Eja::open(&path)?;
    let first = eja.notice(Source::Person, "评审让我有点累")?;
    eja.commit(first, &[keep(None, "也许想讨论评审")], &[], &[], &[])?;
    let concern = eja.view()?.concerns[0].id;
    drop(eja);
    let eja = Eja::open(&path)?;
    let correction = eja.notice(Source::Person, "下次听我说完就好")?;
    eja.commit(correction, &[keep(Some(concern), "先听完")], &[], &[], &[])?;
    let tick = eja.notice(Source::Clock, "第二天")?;
    let view = eja.view()?;
    assert_eq!(view.concerns.len(), 1);
    assert_eq!(view.concerns[0].id, concern);
    assert_eq!(view.concerns[0].text, "先听完");
    assert!(
        matches!(eja.read(concern)?.fact, Fact::Kept { concern: id, ref text } if id == concern && text == "也许想讨论评审")
    );
    assert!(matches!(
        eja.read(correction)?.fact,
        Fact::Noticed {
            source: Source::Person,
            ..
        }
    ));
    eja.commit(tick, &[], &[concern], &[], &[])?;
    assert!(eja.view()?.concerns.is_empty());
    let head = eja.view()?.revision;
    assert!(matches!(
        eja.commit(head, &[keep(Some(concern), "再次修改")], &[], &[], &[]),
        Err(Error::Invalid(_))
    ));
    assert_eq!(eja.concerns(tick, tick)?[0].text, "先听完");
    Ok(())
}

#[test]
fn only_a_fresh_granted_commit_can_issue_a_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let first = Eja::open(&path)?;
    let second = Eja::open(&path)?;
    let basis = first.notice(Source::World, "an observation")?;
    let act = Act {
        capability: "new.tool".into(),
        input: json!({"x": 1}),
    };
    assert!(matches!(
        first.commit(basis, &[], &[], std::slice::from_ref(&act), &[]),
        Err(Error::Invalid(_))
    ));
    let rev = second.notice(Source::Person, "one more thing")?;
    assert!(matches!(
        first.commit(basis, &[], &[], std::slice::from_ref(&act), &["new.tool"]),
        Err(Error::Conflict)
    ));
    let calls = first.commit(rev, &[], &[], std::slice::from_ref(&act), &["new.tool"])?;
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.input, json!({"x":1}));
    assert!(matches!(
        second.commit(rev, &[], &[], &[act], &["new.tool"]),
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
    assert_eq!(resumed.view()?.revision, call.id);
    let unknown = || Outcome::Unknown("host stopped".into());
    let receipt = resumed.finish(call.id, unknown())?;
    assert_eq!(resumed.finish(call.id, unknown())?, receipt);
    let settled = resumed.finish(call.id, Outcome::Observed("verified later".into()))?;
    assert!(settled > receipt);
    assert_eq!(resumed.finish(call.id, unknown())?, receipt);
    assert_eq!(
        resumed.finish(call.id, Outcome::Observed("verified later".into()))?,
        settled
    );
    assert!(matches!(
        resumed.finish(call.id, Outcome::Failed("contradiction".into())),
        Err(Error::Invalid(_))
    ));
    assert_eq!(resumed.view()?.revision, settled);
    Ok(())
}

#[test]
fn attention_is_bounded_and_older_facts_and_concerns_remain_addressable() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let first = eja.notice(Source::Person, "initial")?;
    for n in 0..(PAGE + 1) {
        let head = eja.view()?.revision;
        eja.commit(head, &[keep(None, &n.to_string())], &[], &[], &[])?;
    }
    let view = eja.view()?;
    assert_eq!(view.concerns.len(), PAGE);
    assert_eq!(view.recent.len(), PAGE);
    let earlier = eja.concerns(view.revision, view.concerns.last().unwrap().latest - 1)?;
    assert_eq!(earlier.len(), 1);
    assert_eq!(earlier[0].text, "0");
    assert_eq!(
        eja.history(view.recent.last().unwrap().id - 1)?
            .last()
            .unwrap()
            .id,
        first
    );
    assert!(eja.notice(Source::World, "字".repeat(FACT_BYTES)).is_err());
    assert_eq!(eja.view()?.revision, view.revision);
    Ok(())
}

#[test]
fn simultaneous_commits_issue_only_one_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let first = Eja::open(&path)?;
    let second = Eja::open(&path)?;
    let revision = first.notice(Source::Clock, "wake")?;
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let run = |eja: Eja| {
            barrier.wait();
            match eja.commit(
                revision,
                &[],
                &[],
                &[Act {
                    capability: "one".into(),
                    input: json!(null),
                }],
                &["one"],
            ) {
                Ok(calls) if calls.len() == 1 => true,
                Err(Error::Conflict) => false,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        let a = scope.spawn(move || run(first));
        let b = scope.spawn(move || run(second));
        assert_ne!(a.join().unwrap(), b.join().unwrap());
    });
    assert_eq!(Eja::open(&path)?.view()?.revision, revision + 1);
    Ok(())
}

#[test]
fn one_turn_reconsiders_a_real_receipt_and_stops_at_the_step_budget() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let input = eja.notice(Source::Person, "检查")?;
    let mut calls = 0;
    let stop = eja.turn(
        &["probe"],
        3,
        |view| {
            let latest = &view.recent[0];
            let step = match &latest.fact {
                Fact::Noticed { .. } => Step {
                    act: vec![Act {
                        capability: "probe".into(),
                        input: json!(null),
                    }],
                    ..Step::default()
                },
                Fact::Returned {
                    outcome: Outcome::Failed(reason),
                    ..
                } => Step {
                    keep: vec![keep(None, reason)],
                    ..Step::default()
                },
                _ => panic!("unexpected step"),
            };
            Ok(step)
        },
        |_| {
            calls += 1;
            Outcome::Failed("暂时无法读取".into())
        },
    )?;
    assert_eq!(stop, Stop::Done);
    assert_eq!(calls, 1);
    assert_eq!(eja.view()?.concerns[0].text, "暂时无法读取");
    let stop = eja.turn(
        &[],
        2,
        |_| {
            Ok(Step {
                read: vec![input],
                ..Step::default()
            })
        },
        |_| unreachable!(),
    )?;
    assert_eq!(stop, Stop::StepLimit);
    Ok(())
}

#[test]
fn empty_subject_finishes_and_an_oversized_receipt_closes_its_call() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    assert_eq!(
        eja.turn(&[], 2, |_| unreachable!(), |_| unreachable!())?,
        Stop::Done
    );
    let basis = eja.notice(Source::Person, "test")?;
    let detail = "雨\0\"".repeat(FACT_BYTES);
    eja.turn(
        &["read"],
        1,
        |_| {
            Ok(Step {
                act: vec![Act {
                    capability: "read".into(),
                    input: json!(null),
                }],
                ..Step::default()
            })
        },
        |_| Outcome::Observed(detail.clone()),
    )?;
    let receipt = eja.read(basis + 2)?;
    assert!(serde_json::to_vec(&receipt.fact)?.len() <= FACT_BYTES);
    assert!(
        matches!(receipt.fact, Fact::Returned { outcome: Outcome::Observed(ref s), .. } if s.contains("truncated") && s.contains("sha256:"))
    );
    assert_eq!(
        eja.finish(basis + 1, Outcome::Observed(detail.clone()))?,
        receipt.id
    );
    assert!(eja
        .finish(basis + 1, Outcome::Observed(detail + "!"))
        .is_err());
    Ok(())
}

#[test]
fn an_unknown_receipt_can_be_settled_only_once_by_competing_hosts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("subject.db");
    let seed = Eja::open(&db)?;
    let notice = seed.notice(Source::World, "tick")?;
    let call = seed
        .commit(
            notice,
            &[],
            &[],
            &[Act {
                capability: "read".into(),
                input: json!(null),
            }],
            &["read"],
        )?
        .remove(0);
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
    assert_eq!(e.view()?.revision, unknown + 1);
    Ok(())
}

#[test]
fn commit_records_revisions_and_releases_explicitly() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let basis = eja.notice(Source::Person, "remember aurora and refine if needed")?;
    assert!(!eja.has_encoder());
    assert!(matches!(eja.search("aurora"), Err(Error::Invalid(_))));
    eja.commit(basis, &[keep(None, "aurora")], &[], &[], &[])?;
    let concern = eja.view()?.concerns[0].id;
    eja.commit(
        concern,
        &[keep(Some(concern), "aurora (refined)")],
        &[],
        &[],
        &[],
    )?;
    assert_eq!(eja.view()?.revision, concern + 1);
    assert_eq!(eja.view()?.concerns.len(), 1);
    assert_eq!(eja.view()?.concerns[0].text, "aurora (refined)");
    let head = eja.view()?.revision;
    eja.commit(head, &[], &[concern], &[], &[])?;
    assert!(eja.view()?.concerns.is_empty());
    Ok(())
}
