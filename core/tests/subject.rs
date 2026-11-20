use eja_core::{Choice, Eja, Fact, Judgment, Outcome, Result, Source, FACT_BYTES, PAGE};
use serde_json::json;

fn judgment(basis: i64, choice: Choice) -> Judgment {
    Judgment { basis, choice }
}

#[test]
fn a_judge_can_follow_a_concerns_current_basis_before_acting() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let old = eja.notice(Source::Person, "我喜欢沿河散步")?;
    eja.decide(old, judgment(old, keep(None, "周末的去处")), &[])?;
    let concern = eja.view()?.concerns[0].id;
    let change = eja.notice(Source::Person, "这阵子更想去山里")?;
    eja.decide(
        change,
        judgment(change, keep(Some(concern), "周末的去处")),
        &[],
    )?;
    for _ in 0..PAGE {
        eja.notice(Source::Clock, "tick")?;
    }
    eja.notice(Source::Person, "明天想出去透口气")?;
    let mut steps = 0;
    let stop = eja.turn(&[], 2, |v| {
        steps += 1;
        let basis = v.concerns[0].basis;
        assert_eq!(basis, change);
        assert!(!v.recent.iter().any(|e| e.id == change));
        if steps == 1 {
            assert!(v.recalled.is_none());
            return Ok(judgment(basis, Choice::Recall));
        }
        let memory = v.recalled.as_ref().unwrap();
        assert_eq!(memory.id, change);
        assert!(matches!(&memory.fact, Fact::Noticed { source: Source::Person, text } if text == "这阵子更想去山里"));
        Ok(judgment(memory.id, Choice::Rest))
    }, |_| unreachable!())?;
    assert_eq!(stop, eja_core::Stop::Rest);
    assert_eq!(steps, 2);
    assert!(eja.view()?.recalled.is_none());
    Ok(())
}

#[test]
fn recalling_a_recent_fact_keeps_one_copy_and_uses_one_step() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let basis = eja.notice(Source::Person, "在这里")?;
    let stop = eja.turn(
        &[],
        1,
        |_| Ok(judgment(basis, Choice::Recall)),
        |_| unreachable!(),
    )?;
    assert_eq!(stop, eja_core::Stop::StepLimit);
    let v = eja.view()?;
    assert!(v.recalled.is_none());
    assert!(v.recent.iter().any(|entry| entry.id == basis));
    Ok(())
}

fn keep(concern: Option<i64>, text: &str) -> Choice {
    Choice::Keep {
        concern,
        text: text.into(),
    }
}

#[test]
fn a_correction_changes_later_attention_and_preserves_both_sources() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let eja = Eja::open(&path)?;
    let first = eja.notice(Source::Person, "评审让我有点累")?;
    eja.decide(first, judgment(first, keep(None, "也许想讨论评审")), &[])?;
    let concern = eja.view()?.concerns[0].id;
    drop(eja);
    let eja = Eja::open(&path)?;
    let correction = eja.notice(Source::Person, "下次听我说完就好")?;
    eja.decide(
        correction,
        judgment(correction, keep(Some(concern), "先听完")),
        &[],
    )?;
    let tick = eja.notice(Source::Clock, "第二天")?;
    let view = eja.view()?;
    assert_eq!(view.concerns.len(), 1);
    assert_eq!(view.concerns[0].id, concern);
    assert_eq!(view.concerns[0].text, "先听完");
    assert!(
        matches!(eja.read(concern)?.fact, Fact::Judged { judgment } if judgment.basis == first)
    );
    assert!(matches!(
        eja.read(correction)?.fact,
        Fact::Noticed {
            source: Source::Person,
            ..
        }
    ));
    let revision = view.concerns[0].latest;
    assert!(eja
        .decide(tick, judgment(revision, Choice::Rest), &[])?
        .is_none());
    let head = eja.view()?.revision;
    eja.decide(head, judgment(revision, Choice::Release { concern }), &[])?;
    assert!(eja.view()?.concerns.is_empty());
    let head = eja.view()?.revision;
    assert!(eja
        .decide(head, judgment(head, keep(Some(concern), "再次修改")), &[])
        .is_err());
    assert_eq!(eja.concerns(tick, tick)?[0].text, "先听完");
    Ok(())
}

#[test]
fn only_a_fresh_granted_decision_can_issue_a_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let first = Eja::open(&path)?;
    let second = Eja::open(&path)?;
    let basis = first.notice(Source::World, "an observation")?;
    let wire = json!({"basis":basis,"choice":{"op":"act","capability":"new.tool","input":{"x":1}}});
    let decide = || serde_json::from_value::<Judgment>(wire.clone()).unwrap();
    assert!(first.decide(basis, decide(), &[]).is_err());
    assert!(first
        .decide(basis, judgment(basis + 1, Choice::Rest), &[])
        .is_err());
    let rev = second.notice(Source::Person, "one more thing")?;
    assert!(first.decide(basis, decide(), &["new.tool"]).is_err());
    let call = first.decide(rev, decide(), &["new.tool"])?.unwrap();
    assert_eq!(call.input, json!({"x":1}));
    assert!(second.decide(rev, decide(), &["new.tool"]).is_err());
    assert!(second
        .finish(basis, Outcome::Observed("wrong target".into()))
        .is_err());
    // Simulate the host disappearing after dispatch and before observing the result.
    drop(first);
    drop(second);
    let resumed = Eja::open(&path)?;
    assert_eq!(resumed.view()?.revision, call.id);
    let unknown = || Outcome::Unknown("host stopped".into());
    let receipt = resumed.finish(call.id, unknown())?;
    assert_eq!(resumed.finish(call.id, unknown())?, receipt);
    assert!(resumed
        .finish(call.id, Outcome::Observed("assumed success".into()))
        .is_err());
    assert_eq!(resumed.view()?.revision, receipt);
    Ok(())
}

#[test]
fn attention_is_bounded_and_older_facts_and_concerns_remain_addressable() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let first = eja.notice(Source::Person, "initial")?;
    for n in 0..(PAGE + 1) {
        let head = eja.view()?.revision;
        eja.decide(head, judgment(first, keep(None, &n.to_string())), &[])?;
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
fn simultaneous_decisions_issue_only_one_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let first = Eja::open(&path)?;
    let second = Eja::open(&path)?;
    let revision = first.notice(Source::Clock, "wake")?;
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let run = |eja: Eja| {
            barrier.wait();
            eja.decide(
                revision,
                judgment(
                    revision,
                    Choice::Act {
                        capability: "one".into(),
                        input: json!(null),
                    },
                ),
                &["one"],
            )
            .is_ok()
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
        2,
        |v| {
            let latest = &v.recent[0];
            let choice = match &latest.fact {
                Fact::Noticed { .. } => Choice::Act {
                    capability: "probe".into(),
                    input: json!(null),
                },
                Fact::Returned {
                    outcome: Outcome::Failed(reason),
                    ..
                } => keep(None, reason),
                _ => panic!("unexpected step"),
            };
            Ok(judgment(latest.id, choice))
        },
        |_| {
            calls += 1;
            Outcome::Failed("暂时无法读取".into())
        },
    )?;
    assert_eq!(stop, eja_core::Stop::StepLimit);
    assert_eq!(calls, 1);
    assert_eq!(eja.view()?.concerns[0].text, "暂时无法读取");
    let stop = eja.turn(
        &[],
        1,
        |_| Ok(judgment(input, Choice::Rest)),
        |_| unreachable!(),
    )?;
    assert_eq!(stop, eja_core::Stop::Rest);
    Ok(())
}
