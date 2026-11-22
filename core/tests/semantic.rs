//! Semantic retrieval tests: pure fixture tests run by default; local ONNX tests require `--features onnx`.
use eja_core::{Eja, Encoder, Error, InputKind, Outcome, Source};
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
    for _ in 0..10 {
        subject.notice(Source::World, "日常记录")?;
    }
    let reply = subject.say("已登记 BLUE-74")?;
    let head = subject.revision()?;
    let hits = subject.search("纸质封签 blue-74")?;
    assert_eq!(hits[0].evidence.entry.id, notice);
    assert!(hits.iter().any(|h| h.evidence.entry.id == reply));
    let partial = subject.search("纸质封签 次日")?;
    assert_eq!(partial[0].evidence.entry.id, notice);
    assert_eq!(partial[0].similarity, 0.5);
    assert!(subject.search("不存在的关键词")?.is_empty());
    assert!(matches!(subject.search(" "), Err(Error::Invalid(_))));
    assert_eq!(subject.revision()?, head);
    Ok(())
}

#[test]
fn search_and_evidence_present_settled_receipts_without_polluting_journal() -> Result<()> {
    let subject = Eja::open_with_encoder(":memory:", Fixture { id: "v1", dims: 2 })?;
    assert!(subject.has_encoder());
    let basis = subject.notice(Source::Person, "scan β")?;
    let call = subject.act(basis, "scan", Value::Null, &["scan"])?;
    let unknown = subject.finish(call.id, Outcome::Unknown("β lost connection".into()))?;
    let settled = subject.finish(call.id, Outcome::Observed("barcode accepted".into()))?;
    for n in 0..10 {
        subject.notice(Source::World, format!("α unrelated event {n}"))?;
    }
    let trigger = subject.notice(Source::Person, "α what happened?")?;
    assert!(matches!(subject.search("  "), Err(Error::Invalid(_))));
    let ev = subject.evidence(unknown.id)?;
    assert_eq!(ev.entry.id, unknown.id);
    assert_eq!(ev.latest.as_ref().unwrap().id, settled.id);
    let hits = subject.search("β")?;
    let hit = hits
        .iter()
        .find(|item| item.evidence.entry.id == unknown.id)
        .expect("unknown receipt matched by search");
    assert_eq!(hit.evidence.latest.as_ref().unwrap().id, settled.id);
    assert_eq!(subject.revision()?, trigger);
    Ok(())
}

#[cfg(feature = "onnx")]
mod onnx_tests {
    use super::Result;
    use eja_core::{Eja, Fact, OnnxEncoder, Outcome, Source};
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
        assert!(subject.recent(8)?.iter().all(|entry| entry.id != old));
        assert!(subject.concerns(subject.revision()?)?.is_empty());
        let recalled = &subject.search(cue)?[0].evidence.entry;
        assert_eq!(recalled.id, old);
        assert!(matches!(
            &recalled.fact,
            Fact::Noticed {
                source: Source::Person,
                ..
            }
        ));
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
        a.notice(Source::Person, "我们来谈谈通勤")?;
        let text = "以后坐地铁回家";
        let kept = a.keep(None, text)?;
        let released = a.release(kept.id)?;
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let run = |subject: Eja| -> Result<()> {
                barrier.wait();
                let hits = subject.search(text)?;
                let hit = hits
                    .iter()
                    .find(|item| item.evidence.entry.id == kept.id)
                    .expect("old keep");
                assert_eq!(hit.evidence.latest.as_ref().unwrap().id, released.id);
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
            assert!(eja.recent(8)?.iter().all(|entry| entry.id != target));
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
        let call = subject.act(basis, "scan", serde_json::Value::Null, &["scan"])?;
        let query = "Parcel delivery scanner lost its network connection.";
        let missing = subject.finish(call.id, Outcome::Unknown(query.into()))?;
        drop(subject);
        let subject = Eja::open_with_encoder(&path, encoder)?;
        let resolved = subject.finish(call.id, Outcome::Observed("barcode accepted".into()))?;
        for idx in 0..10 {
            subject.notice(Source::World, format!("unrelated event {idx}"))?;
        }
        subject.notice(Source::Person, "What happened earlier?")?;
        let hits = subject.search(query)?;
        assert_eq!(hits[0].evidence.entry.id, missing.id);
        assert_eq!(hits[0].evidence.latest.as_ref().unwrap().id, resolved.id);
        let ev = subject.evidence(missing.id)?;
        assert_eq!(ev.entry.id, missing.id);
        assert_eq!(ev.latest.as_ref().unwrap().id, resolved.id);
        Ok(())
    }
}
