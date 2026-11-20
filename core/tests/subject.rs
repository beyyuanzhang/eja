use eja_core::{Choice, Eja, Error, Fact, Judgment, Outcome, Source, FACT_BYTES, PAGE};
use serde_json::json;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

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
    let trigger = eja.notice(Source::Person, "明天想出去透口气")?;
    let mut steps = 0;
    let stop = eja.turn(&[], 2, |v| {
        steps += 1;
        let basis = v.concerns[0].basis;
        assert_eq!(basis, change);
        assert!(!v.recent.iter().any(|e| e.id == change));
        if steps == 1 {
            assert!(v.read.is_none());
            return Ok(judgment(trigger, Choice::Read { fact: basis }));
        }
        let memory = v.read.as_ref().unwrap();
        assert_eq!(memory.entry.id, change);
        assert!(memory.latest.is_none());
        assert!(matches!(&memory.entry.fact, Fact::Noticed { source: Source::Person, text } if text == "这阵子更想去山里"));
        Ok(judgment(memory.entry.id, Choice::Rest))
    }, |_| unreachable!())?;
    assert_eq!(stop, eja_core::Stop::Rest);
    assert_eq!(steps, 2);
    assert!(eja.view()?.read.is_none());
    Ok(())
}

#[test]
fn reading_any_fact_returns_evidence_and_rejects_unknown_targets() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let basis = eja.notice(Source::Person, "在这里")?;
    assert!(matches!(
        eja.decide(
            basis,
            judgment(basis, Choice::Read { fact: basis + 1 }),
            &[]
        ),
        Err(Error::Invalid(_))
    ));
    let stop = eja.turn(
        &[],
        1,
        |_| Ok(judgment(basis, Choice::Read { fact: basis })),
        |_| unreachable!(),
    )?;
    assert_eq!(stop, eja_core::Stop::StepLimit);
    let v = eja.view()?;
    assert_eq!(v.read.as_ref().unwrap().entry.id, basis);
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
    assert!(matches!(
        eja.decide(head, judgment(head, keep(Some(concern), "再次修改")), &[]),
        Err(Error::Invalid(_))
    ));
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
    assert!(matches!(
        first.decide(basis, decide(), &[]),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        first.decide(basis, judgment(basis + 1, Choice::Rest), &[]),
        Err(Error::Invalid(_))
    ));
    let rev = second.notice(Source::Person, "one more thing")?;
    assert!(matches!(
        first.decide(basis, decide(), &["new.tool"]),
        Err(Error::Conflict)
    ));
    let call = first.decide(rev, decide(), &["new.tool"])?.unwrap();
    assert_eq!(call.input, json!({"x":1}));
    assert!(matches!(
        second.decide(rev, decide(), &["new.tool"]),
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
            match eja.decide(
                revision,
                judgment(
                    revision,
                    Choice::Act {
                        capability: "one".into(),
                        input: json!(null),
                    },
                ),
                &["one"],
            ) {
                Ok(Some(_)) => true,
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

#[test]
fn identical_actions_require_distinct_basis_evidence() -> Result<()> {
    let e = Eja::open(":memory:")?;
    let act = |basis| {
        judgment(
            basis,
            Choice::Act {
                capability: "write".into(),
                input: json!({"text": "\n雪"}),
            },
        )
    };
    let first = e.notice(Source::Person, "first")?;
    let call = e.decide(first, act(first), &["write"])?.unwrap();
    let receipt = e.finish(call.id, Outcome::Observed("ok".into()))?;
    assert!(matches!(
        e.decide(receipt, act(first), &["write"]),
        Err(Error::Invalid(_))
    ));
    let second = e.notice(Source::Person, "again")?;
    assert!(matches!(
        e.decide(second, act(first), &["write"]),
        Err(Error::Invalid(_))
    ));
    assert!(e.decide(second, act(second), &["write"])?.is_some());
    Ok(())
}

#[test]
fn empty_subject_rests_and_an_oversized_receipt_closes_its_call() -> Result<()> {
    let e = Eja::open(":memory:")?;
    assert_eq!(
        e.turn(&[], 2, |_| unreachable!(), |_| unreachable!())?,
        eja_core::Stop::Rest
    );
    let basis = e.notice(Source::Person, "test")?;
    let detail = "雨\0\"".repeat(FACT_BYTES);
    e.turn(
        &["read"],
        1,
        |_| {
            Ok(judgment(
                basis,
                Choice::Act {
                    capability: "read".into(),
                    input: json!(null),
                },
            ))
        },
        |_| Outcome::Observed(detail.clone()),
    )?;
    let receipt = e.read(basis + 2)?;
    assert!(serde_json::to_vec(&receipt.fact)?.len() <= FACT_BYTES);
    assert!(
        matches!(receipt.fact, Fact::Returned { outcome: Outcome::Observed(ref s), .. } if s.contains("truncated") && s.contains("sha256:"))
    );
    assert_eq!(
        e.finish(basis + 1, Outcome::Observed(detail.clone()))?,
        receipt.id
    );
    assert!(e
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
        .decide(
            notice,
            judgment(
                notice,
                Choice::Act {
                    capability: "read".into(),
                    input: json!(null),
                },
            ),
            &["read"],
        )?
        .unwrap();
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
fn decide_commits_meaningful_judgments_without_silent_drops_and_search_needs_encoder() -> Result<()>
{
    let e = Eja::open(":memory:")?;
    let basis = e.notice(Source::Person, "remember aurora and refine if needed")?;
    assert!(e.view()?.related.is_none());
    assert!(matches!(
        e.decide(
            basis,
            judgment(
                basis,
                Choice::Search {
                    cue: "aurora".into()
                }
            ),
            &[]
        ),
        Err(Error::Invalid(_))
    ));
    e.decide(basis, judgment(basis, keep(None, "aurora")), &[])?;
    let concern = e.view()?.concerns[0].id;
    // Revising with the same basis or releasing is committed explicitly rather than swallowed.
    e.decide(
        concern,
        judgment(basis, keep(Some(concern), "aurora (refined)")),
        &[],
    )?;
    assert_eq!(e.view()?.revision, concern + 1);
    assert_eq!(e.view()?.concerns.len(), 1);
    assert_eq!(e.view()?.concerns[0].text, "aurora (refined)");
    let head = e.view()?.revision;
    e.decide(head, judgment(basis, Choice::Release { concern }), &[])?;
    assert!(e.view()?.concerns.is_empty());
    Ok(())
}
