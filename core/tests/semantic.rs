//! Semantic retrieval tests: pure fixture tests run by default; local ONNX tests require `--features onnx`.
use eja_core::{Act, Eja, Encoder, Error, InputKind, Outcome, Source, Step, Stop, PAGE};
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
fn keyword_search_finds_older_evidence_without_writing_to_the_journal() -> Result<()> {
    let subject = Eja::open(":memory:")?;
    assert!(!subject.has_encoder());
    let notice = subject.notice(Source::World, "首次交班，纸质封签短码 BLUE-74")?;
    for _ in 0..eja_core::PAGE {
        subject.notice(Source::World, "日常记录")?;
    }
    let head = subject.view()?.revision;
    assert_eq!(
        subject.search("纸质封签 blue-74")?[0].evidence.entry.id,
        notice
    );
    assert!(subject.search("纸质封签 次日")?.is_empty());
    assert!(matches!(subject.search(" "), Err(Error::Invalid(_))));
    let mut first_step = true;
    subject.turn(
        &[],
        2,
        |view| {
            if std::mem::take(&mut first_step) {
                Ok(Step {
                    search: vec!["纸质封签 短码".into()],
                    ..Step::default()
                })
            } else {
                assert_eq!(view.searches[0].matches[0].evidence.entry.id, notice);
                Ok(Step::default())
            }
        },
        |_| unreachable!(),
    )?;
    assert_eq!(subject.view()?.revision, head);
    Ok(())
}

#[test]
fn turn_search_and_read_present_settled_evidence_without_polluting_journal() -> Result<()> {
    let subject = Eja::open_with_encoder(":memory:", Fixture { id: "v1", dims: 2 })?;
    assert!(subject.has_encoder());
    let basis = subject.notice(Source::Person, "scan β")?;
    let call = subject
        .commit(
            basis,
            &Step {
                act: vec![Act {
                    capability: "scan".into(),
                    input: Value::Null,
                }],
                ..Step::default()
            },
            &["scan"],
        )?
        .remove(0);
    let unknown = subject.finish(call.id, Outcome::Unknown("β lost connection".into()))?;
    let settled = subject.finish(call.id, Outcome::Observed("barcode accepted".into()))?;
    for n in 0..PAGE + 1 {
        subject.notice(Source::World, format!("α unrelated event {n}"))?;
    }
    let trigger = subject.notice(Source::Person, "α what happened?")?;
    let pure_view = subject.view()?;
    assert!(pure_view.reads.is_empty());
    assert!(pure_view.searches.is_empty());
    assert!(matches!(
        subject.turn(
            &[],
            2,
            |_| Ok(Step {
                search: vec!["  ".into()],
                ..Step::default()
            }),
            |_| unreachable!(),
        ),
        Err(Error::Invalid(_))
    ));
    let mut steps = 0;
    let stop = subject.turn(
        &[],
        3,
        |view| {
            steps += 1;
            if steps == 1 {
                return Ok(Step {
                    read: vec![unknown],
                    search: vec!["β".into()],
                    ..Step::default()
                });
            }
            assert_eq!(view.reads.len(), 1);
            assert_eq!(view.reads[0].entry.id, unknown);
            assert_eq!(view.reads[0].latest.as_ref().unwrap().id, settled);
            assert_eq!(view.searches.len(), 1);
            assert_eq!(view.searches[0].cue, "β");
            let hit = view.searches[0]
                .matches
                .iter()
                .find(|item| item.evidence.entry.id == unknown)
                .expect("unknown receipt matched by search");
            assert_eq!(hit.evidence.latest.as_ref().unwrap().id, settled);
            Ok(Step::default())
        },
        |_| unreachable!(),
    )?;
    assert_eq!(stop, Stop::Done);
    assert_eq!(steps, 2);
    assert_eq!(subject.view()?.revision, trigger);
    Ok(())
}

#[cfg(feature = "onnx")]
mod onnx_tests {
    use super::Result;
    use eja_core::{Act, Eja, Fact, Keep, OnnxEncoder, Outcome, Source, Step, Stop, PAGE};
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
    fn an_unseen_fact_returns_on_a_paraphrase_without_a_concern() -> Result<()> {
        let encoder = load_encoder()?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("subject.db");
        let subject = Eja::open_with_encoder(&path, encoder.clone())?;
        let old = subject.notice(Source::Person, "备用钥匙在楼下花盆底部。")?;
        for idx in 0..300 {
            subject.notice(Source::World, format!("仓库盘点第{idx}批单据。"))?;
        }
        let cue = "被锁在屋外进不了家，哪里有开门的东西？";
        subject.notice(Source::Person, cue)?;
        assert!(subject.view()?.recent.iter().all(|entry| entry.id != old));
        assert!(subject.view()?.concerns.is_empty());
        let mut steps = 0;
        let stop = subject.turn(
            &[],
            2,
            |view| {
                steps += 1;
                if steps == 1 {
                    return Ok(Step {
                        search: vec![cue.into()],
                        ..Step::default()
                    });
                }
                let recalled = &view.searches[0].matches[0].evidence.entry;
                assert_eq!(recalled.id, old);
                assert!(matches!(
                    &recalled.fact,
                    Fact::Noticed {
                        source: Source::Person,
                        ..
                    }
                ));
                Ok(Step::default())
            },
            |_| unreachable!(),
        )?;
        assert_eq!(stop, Stop::Done);
        drop(subject);
        let subject = Eja::open_with_encoder(path, encoder)?;
        assert!(subject
            .search(cue)?
            .iter()
            .any(|hit| hit.evidence.entry.id == old));
        let newest = subject.notice(Source::Person, "备用钥匙在楼下花盆底部。")?;
        assert_eq!(
            subject.search("备用钥匙在楼下花盆底部。")?[0]
                .evidence
                .entry
                .id,
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
        a.commit(
            first,
            &Step {
                keep: vec![Keep {
                    concern: None,
                    text: text.into(),
                }],
                ..Step::default()
            },
            &[],
        )?;
        let note = first + 1;
        a.commit(
            note,
            &Step {
                release: vec![note],
                ..Step::default()
            },
            &[],
        )?;
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let run = |subject: Eja| -> Result<()> {
                barrier.wait();
                let hits = subject.search(text)?;
                let hit = hits
                    .iter()
                    .find(|item| item.evidence.entry.id == note)
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
            assert!(view.recent.iter().all(|entry| entry.id != target));
            let hits = eja.search(cue)?;
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
            .commit(
                basis,
                &Step {
                    act: vec![Act {
                        capability: "scan".into(),
                        input: serde_json::Value::Null,
                    }],
                    ..Step::default()
                },
                &["scan"],
            )?
            .remove(0);
        let query = "Parcel delivery scanner lost its network connection.";
        let missing = subject.finish(call.id, Outcome::Unknown(query.into()))?;
        drop(subject);
        let subject = Eja::open_with_encoder(&path, encoder)?;
        let resolved = subject.finish(call.id, Outcome::Observed("barcode accepted".into()))?;
        for idx in 0..PAGE + 1 {
            subject.notice(Source::World, format!("unrelated event {idx}"))?;
        }
        subject.notice(Source::Person, "What happened earlier?")?;
        let mut steps = 0;
        let stop = subject.turn(
            &[],
            2,
            |view| {
                steps += 1;
                if steps == 1 {
                    return Ok(Step {
                        read: vec![missing],
                        search: vec![query.into()],
                        ..Step::default()
                    });
                }
                assert_eq!(view.searches[0].matches[0].evidence.entry.id, missing);
                assert_eq!(
                    view.searches[0].matches[0]
                        .evidence
                        .latest
                        .as_ref()
                        .unwrap()
                        .id,
                    resolved
                );
                assert_eq!(view.reads[0].entry.id, missing);
                assert_eq!(view.reads[0].latest.as_ref().unwrap().id, resolved);
                Ok(Step::default())
            },
            |_| unreachable!(),
        )?;
        assert_eq!(stop, Stop::Done);
        Ok(())
    }
}
