//! Opt in with the pinned local model installed. All similarity scores come from the configured encoder.
use eja_core::{Choice, Eja, Fact, Judgment, Result, Source};
use std::sync::Barrier;

#[test]
#[ignore = "requires core/scripts/install-memory-model.sh"]
fn an_unseen_fact_returns_on_a_paraphrase_without_a_basis_or_concern() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let e = Eja::open(&path)?;
    let old = e.notice(Source::Person, "备用钥匙在楼下花盆底部。")?;
    for i in 0..300 {
        e.notice(Source::World, format!("仓库盘点第{i}批单据。"))?;
    }
    let cue = "被锁在屋外进不了家，哪里有开门的东西？";
    e.notice(Source::Person, cue)?;
    assert!(e.view()?.recent.iter().all(|f| f.id != old));
    assert!(e.view()?.concerns.is_empty());
    let stop = e.turn(
        &[],
        1,
        |v| {
            let matches = v.related.as_ref().expect("test requires an encoder");
            let recalled = &matches[0].entry;
            assert_eq!(recalled.id, old);
            assert!(matches!(
                &recalled.fact,
                Fact::Noticed {
                    source: Source::Person,
                    ..
                }
            ));
            Ok(Judgment {
                basis: recalled.id,
                choice: Choice::Rest,
            })
        },
        |_| unreachable!(),
    )?;
    assert_eq!(stop, eja_core::Stop::Rest);
    drop(e);
    let e = Eja::open(path)?;
    assert_eq!(e.view()?.related.unwrap()[0].entry.id, old);
    let newest = e.notice(Source::Person, "备用钥匙在楼下花盆底部。")?;
    assert_eq!(e.search("备用钥匙在楼下花盆底部。")?[0].entry.id, newest);
    Ok(())
}

#[test]
#[ignore = "requires core/scripts/install-memory-model.sh"]
fn backfill_is_atomic_across_concurrent_indexers_and_preserves_corrections() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let a = Eja::open(&path)?;
    let b = Eja::open(&path)?;
    let first = a.notice(Source::Person, "我们来谈谈通勤")?;
    let text = "以后坐地铁回家";
    a.decide(
        first,
        Judgment {
            basis: first,
            choice: Choice::Keep {
                concern: None,
                text: text.into(),
            },
        },
        &[],
    )?;
    let note = first + 1;
    a.decide(
        note,
        Judgment {
            basis: note,
            choice: Choice::Release { concern: note },
        },
        &[],
    )?;
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let run = |e: Eja| -> Result<()> {
            barrier.wait();
            let hits = e.search(text)?;
            let hit = hits.iter().find(|h| h.entry.id == note).expect("old keep");
            assert_eq!(hit.revision_of_concern.as_ref().unwrap().id, note + 1);
            Ok(())
        };
        let left = scope.spawn(move || run(a));
        let right = scope.spawn(move || run(b));
        left.join().unwrap()?;
        right.join().unwrap()
    })
}

#[test]
#[ignore = "requires the multilingual model installed by core/scripts/install-memory-model.sh"]
fn cross_language_queries_recover_original_facts_outside_the_recent_page() -> Result<()> {
    let eja = Eja::open(":memory:")?;
    let examples = [
        (
            "备用钥匙藏在蓝色花盆下面。",
            "Where did I hide the spare key to my house?",
        ),
        (
            "My violin lesson moved from Monday to Thursday.",
            "小提琴课改到了星期几？",
        ),
        (
            "La bicicleta roja está guardada en el garaje.",
            "Où est rangé mon vélo rouge ?",
        ),
        (
            "パスポートは寝室の金庫に入っています。",
            "Where did I put my passport?",
        ),
    ];
    let mut targets = Vec::new();
    for (text, _) in examples {
        targets.push(eja.notice(Source::Person, text)?);
    }
    for i in 0..128 {
        eja.notice(Source::World, format!("Inventory report for parcel {i}"))?;
    }
    for ((text, cue), target) in examples.into_iter().zip(targets) {
        eja.notice(Source::Person, cue)?;
        let view = eja.view()?;
        assert!(view.recent.iter().all(|e| e.id != target));
        let hits = view.related.expect("test requires a multilingual encoder");
        let rank = hits
            .iter()
            .position(|hit| hit.entry.id == target)
            .unwrap_or_else(|| panic!("{cue}: {hits:?}"));
        eprintln!("{cue}: rank {}", rank + 1);
        assert!(
            matches!(&hits[rank].entry.fact, Fact::Noticed {text: original,..} if original==text)
        );
    }
    Ok(())
}

#[test]
fn independent_encoder_spaces_share_one_journal() -> Result<()> {
    use eja_core::{Encoder, InputKind};
    struct Fixture(usize);
    impl Encoder for Fixture {
        fn identity(&self) -> &str {
            "fixture"
        }
        fn dimensions(&self) -> usize {
            self.0
        }
        fn encode(&mut self, _: InputKind, inputs: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(inputs
                .iter()
                .map(|s| {
                    let mut vector = vec![0.; self.0];
                    vector[usize::from(s.starts_with('β'))] = 1.;
                    vector
                })
                .collect())
        }
    }
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("subject.db");
    let first = Eja::open_with_encoder(&db, Fixture(2))?;
    let id = first.notice(Source::Person, "β")?;
    assert_eq!(first.search("β")?[0].entry.id, id);
    let second = Eja::open_with_encoder(&db, Fixture(5))?;
    let other = second.notice(Source::Person, "α")?;
    for handle in [&first, &second] {
        assert_eq!(handle.search("β")?[0].entry.id, id);
        assert_eq!(handle.search("α")?[0].entry.id, other);
    }
    Ok(())
}
