//! Semantic retrieval tests: pure fixture tests run by default; local ONNX tests require `--features onnx`.
use eja_core::{Choice, Eja, Encoder, Error, InputKind, Judgment, Outcome, Source, PAGE};
use serde_json::Value;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Fixture {
    id: &'static str,
    dims: usize,
}

impl Encoder for Fixture {
    fn identity(&self) -> &str {
        self.id
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    fn encode(
        &mut self,
        _: InputKind,
        inputs: &[&str],
    ) -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(inputs
            .iter()
            .map(|text| {
                let mut vector = vec![0.0; self.dims];
                let slot = usize::from(text.contains('β'));
                vector[slot] = 1.0;
                vector
            })
            .collect())
    }
}

#[test]
fn switching_encoder_rebuilds_derived_index_without_touching_journal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("subject.db");
    let first = Eja::open_with_encoder(&db, Fixture { id: "v1", dims: 2 })?;
    let beta = first.notice(Source::Person, "β")?;
    assert_eq!(first.search("β")?[0].evidence.entry.id, beta);
    drop(first);

    let second = Eja::open_with_encoder(&db, Fixture { id: "v2", dims: 5 })?;
    let alpha = second.notice(Source::Person, "α")?;
    assert_eq!(second.search("β")?[0].evidence.entry.id, beta);
    assert_eq!(second.search("α")?[0].evidence.entry.id, alpha);
    Ok(())
}

#[test]
fn repeated_passages_occupy_at_most_one_slot_in_top_results() -> Result<()> {
    let subject = Eja::open_with_encoder(":memory:", Fixture { id: "v1", dims: 2 })?;
    let unique = subject.notice(Source::Person, "β unique detail")?;
    let mut last_dup = 0;
    for _ in 0..8 {
        last_dup = subject.notice(Source::World, "β repeated boilerplate")?;
    }
    let hits = subject.search("β")?;
    let ids: Vec<_> = hits.iter().map(|hit| hit.evidence.entry.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&unique));
    assert!(ids.contains(&last_dup));
    Ok(())
}

#[test]
fn search_choice_queries_history_and_read_choice_skips_auto_search() -> Result<()> {
    let subject = Eja::open_with_encoder(":memory:", Fixture { id: "v1", dims: 2 })?;
    let basis = subject.notice(Source::Person, "scan β")?;
    let call = subject
        .decide(
            basis,
            Judgment {
                basis,
                choice: Choice::Act {
                    capability: "scan".into(),
                    input: Value::Null,
                },
            },
            &["scan"],
        )?
        .unwrap();
    let unknown = subject.finish(call.id, Outcome::Unknown("β lost connection".into()))?;
    let settled = subject.finish(call.id, Outcome::Observed("barcode accepted".into()))?;
    for n in 0..PAGE + 1 {
        subject.notice(Source::World, format!("α unrelated event {n}"))?;
    }
    let trigger = subject.notice(Source::Person, "α what happened?")?;
    assert!(matches!(
        subject.decide(
            trigger,
            Judgment {
                basis: trigger,
                choice: Choice::Search { cue: "  ".into() },
            },
            &[]
        ),
        Err(Error::Invalid(_))
    ));
    subject.decide(
        trigger,
        Judgment {
            basis: trigger,
            choice: Choice::Search { cue: "β".into() },
        },
        &[],
    )?;
    let view = subject.view()?;
    let hits = view.related.unwrap();
    let hit = hits
        .iter()
        .find(|item| item.evidence.entry.id == unknown)
        .expect("unknown receipt matched by search");
    assert_eq!(hit.evidence.latest.as_ref().unwrap().id, settled);

    subject.decide(
        view.revision,
        Judgment {
            basis: trigger,
            choice: Choice::Read { fact: unknown },
        },
        &[],
    )?;
    let view = subject.view()?;
    let read = view.read.unwrap();
    assert_eq!(read.entry.id, unknown);
    assert_eq!(read.latest.unwrap().id, settled);
    assert!(view.related.unwrap().is_empty());
    Ok(())
}

#[cfg(feature = "onnx")]
mod onnx_tests {
    use super::Result;
    use eja_core::{Choice, Eja, Fact, Judgment, OnnxEncoder, Outcome, Source, PAGE};
    use std::{path::PathBuf, sync::Barrier};

    fn load_encoder() -> Result<OnnxEncoder> {
        let dir = std::env::var_os("EJA_MEMORY_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join(".models")
                    .join("multilingual-e5-small")
            });
        Ok(OnnxEncoder::load(dir)?)
    }

    #[test]
    #[ignore = "requires core/scripts/install-memory-model.sh"]
    fn an_unseen_fact_returns_on_a_paraphrase_without_a_basis_or_concern() -> Result<()> {
        let encoder = load_encoder()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("subject.db");
        let e = Eja::open_with_encoder(&path, encoder.clone())?;
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
                let recalled = &matches[0].evidence.entry;
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
        let e = Eja::open_with_encoder(path, encoder)?;
        assert_eq!(e.view()?.related.unwrap()[0].evidence.entry.id, old);
        let newest = e.notice(Source::Person, "备用钥匙在楼下花盆底部。")?;
        assert_eq!(
            e.search("备用钥匙在楼下花盆底部。")?[0].evidence.entry.id,
            newest
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires core/scripts/install-memory-model.sh"]
    fn backfill_is_atomic_across_concurrent_indexers_and_preserves_corrections() -> Result<()> {
        let encoder = load_encoder()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("subject.db");
        let a = Eja::open_with_encoder(&path, encoder.clone())?;
        let b = Eja::open_with_encoder(&path, encoder)?;
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
                let hit = hits
                    .iter()
                    .find(|h| h.evidence.entry.id == note)
                    .expect("old keep");
                assert_eq!(hit.evidence.latest.as_ref().unwrap().id, note + 1);
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
        let eja = Eja::open_with_encoder(":memory:", load_encoder()?)?;
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
                .position(|hit| hit.evidence.entry.id == target)
                .unwrap_or_else(|| panic!("{cue}: {hits:?}"));
            eprintln!("{cue}: rank {}", rank + 1);
            assert!(
                matches!(&hits[rank].evidence.entry.fact, Fact::Noticed { text: original, .. } if original == text)
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires multilingual encoder"]
    fn focused_search_and_explicit_read_present_settled_evidence() -> Result<()> {
        let encoder = load_encoder()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("subject.db");
        let subject = Eja::open_with_encoder(&path, encoder.clone())?;
        let basis = subject.notice(Source::Person, "Scan the parcel.")?;
        let call = subject
            .decide(
                basis,
                Judgment {
                    basis,
                    choice: Choice::Act {
                        capability: "scan".into(),
                        input: serde_json::Value::Null,
                    },
                },
                &["scan"],
            )?
            .unwrap();
        let query = "Parcel delivery scanner lost its network connection.";
        let missing = subject.finish(call.id, Outcome::Unknown(query.into()))?;
        drop(subject);
        let subject = Eja::open_with_encoder(&path, encoder)?;
        let resolved = subject.finish(call.id, Outcome::Observed("barcode accepted".into()))?;
        for n in 0..PAGE + 1 {
            subject.notice(Source::World, format!("unrelated event {n}"))?;
        }
        let trigger = subject.notice(Source::Person, "What happened earlier?")?;
        subject.decide(
            trigger,
            Judgment {
                basis: trigger,
                choice: Choice::Search { cue: query.into() },
            },
            &[],
        )?;
        let view = subject.view()?;
        let hits = view.related.unwrap();
        assert_eq!(hits[0].evidence.entry.id, missing);
        assert_eq!(hits[0].evidence.latest.as_ref().unwrap().id, resolved);
        subject.decide(
            view.revision,
            Judgment {
                basis: trigger,
                choice: Choice::Read { fact: missing },
            },
            &[],
        )?;
        let view = subject.view()?;
        let read = view.read.unwrap();
        assert_eq!(read.entry.id, missing);
        assert_eq!(read.latest.unwrap().id, resolved);
        Ok(())
    }
}
