use eja_core::{Eja, Fact, Outcome, Source, Timestamp};
use eja_memory::{Encoder, InputKind, MemoryIndex};
use serde_json::json;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct ToyEncoder {
    id: &'static str,
    dims: usize,
}

impl Encoder for ToyEncoder {
    fn identity(&self) -> &str {
        self.id
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn encode(
        &mut self,
        _kind: InputKind,
        texts: &[&str],
    ) -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vec = vec![0.01f32; self.dims];
                if text.contains("茶") || text.contains("饮") {
                    vec[0] = 1.0;
                }
                if text.contains("锁") || text.contains("钥匙") {
                    vec[1] = 1.0;
                }
                if text.contains("重复") {
                    vec[2] = 1.0;
                }
                vec
            })
            .collect())
    }
}

#[test]
fn keyword_search_finds_older_evidence_without_writing_to_the_journal() -> TestResult {
    let eja = Eja::open(":memory:")?;
    let mut memory = MemoryIndex::keyword();
    let key_fact = eja.notice(Source::Person, "备用钥匙放在门口绿植花盆下面")?;
    for idx in 0..20 {
        eja.notice(Source::World, format!("无关事件 {idx}"))?;
    }
    let rev_before = eja.revision()?;
    let hits = memory.search(&eja, "备用钥匙，花盆")?;
    assert_eq!(hits[0].evidence.entry.id, key_fact);
    assert_eq!(eja.revision()?, rev_before);
    Ok(())
}

#[test]
fn repeated_passages_occupy_at_most_one_slot_in_top_results() -> TestResult {
    let dir = tempfile::tempdir()?;
    let eja = Eja::open(dir.path().join("subject.db"))?;
    let mut memory = MemoryIndex::open(
        dir.path().join("memory.db"),
        ToyEncoder {
            id: "toy-v1",
            dims: 4,
        },
    )?;
    for _ in 0..15 {
        eja.notice(Source::World, "重复心跳日志")?;
    }
    let distinct = eja.notice(Source::Person, "重复但有关键内容：绿茶")?;
    let hits = memory.search(&eja, "重复")?;
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|hit| hit.evidence.entry.id == distinct));
    Ok(())
}

#[test]
fn search_and_evidence_present_settled_receipts_without_polluting_journal() -> TestResult {
    let dir = tempfile::tempdir()?;
    let eja = Eja::open(dir.path().join("subject.db"))?;
    let mut memory = MemoryIndex::open(
        dir.path().join("memory.db"),
        ToyEncoder {
            id: "toy-v1",
            dims: 4,
        },
    )?;
    let rev = eja.notice(Source::Person, "帮我检查门锁")?;
    let call = eja.act(rev, "door.check", json!({"target": "门锁"}))?;
    eja.finish(call.id, Outcome::Unknown("超时".into()))?;
    let settled = eja.finish(call.id, Outcome::Observed("门锁已关闭".into()))?;
    let rev_before = eja.revision()?;
    let hits = memory.search(&eja, "钥匙")?;
    assert_eq!(eja.revision()?, rev_before);
    let action_hit = hits
        .iter()
        .find(|hit| hit.evidence.entry.id == call.id)
        .expect("action hit");
    assert_eq!(action_hit.evidence.latest.as_ref().unwrap().id, settled.id);
    assert!(matches!(
        action_hit.evidence.latest.as_ref().unwrap().fact,
        Fact::Returned {
            outcome: Outcome::Observed(ref detail),
            ..
        } if detail == "门锁已关闭"
    ));
    Ok(())
}

#[test]
fn switching_encoder_rebuilds_derived_index_without_touching_journal() -> TestResult {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("subject.db");
    let mem_path = dir.path().join("memory.db");
    let eja = Eja::open(&db_path)?;
    let first = eja.notice(Source::Person, "喜欢喝绿茶")?;
    let rev_before = eja.revision()?;

    let mut mem_v1 = MemoryIndex::open(
        &mem_path,
        ToyEncoder {
            id: "toy-v1",
            dims: 4,
        },
    )?;
    assert_eq!(mem_v1.search(&eja, "饮品")?[0].evidence.entry.id, first);
    drop(mem_v1);

    let mut mem_v2 = MemoryIndex::open(
        &mem_path,
        ToyEncoder {
            id: "toy-v2",
            dims: 8,
        },
    )?;
    assert_eq!(mem_v2.search(&eja, "饮品")?[0].evidence.entry.id, first);
    assert_eq!(eja.revision()?, rev_before);
    Ok(())
}

#[test]
fn stale_vector_cache_falls_back_to_keywords_at_the_same_revision() -> TestResult {
    let dir = tempfile::tempdir()?;
    let eja = Eja::open(dir.path().join("subject.db"))?;
    let mem_path = dir.path().join("memory.db");
    let old = eja.notice(Source::Person, "喜欢喝绿茶")?;

    let mut stale = MemoryIndex::open(
        &mem_path,
        ToyEncoder {
            id: "toy-v1",
            dims: 4,
        },
    )?;
    let hits = stale.search(&eja, "饮品")?;
    assert_eq!(hits[0].evidence.entry.id, old);
    assert!(MemoryIndex::keyword().search(&eja, "饮品")?.is_empty());

    let mut fresh = MemoryIndex::open(
        &mem_path,
        ToyEncoder {
            id: "toy-v2",
            dims: 8,
        },
    )?;
    let new = eja.notice(Source::Person, "备用钥匙放在抽屉里")?;
    assert_eq!(fresh.search(&eja, "钥匙")?.len(), 2);
    let hits = stale.search(&eja, "钥匙")?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].evidence.entry.id, new);
    assert!(stale.search_at(&eja, "钥匙", old)?.is_empty());
    assert_eq!(fresh.search(&eja, "钥匙")?.len(), 2);
    Ok(())
}

#[test]
fn reused_cache_checks_the_full_indexed_journal_prefix() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("memory.db");
    let encoder = || ToyEncoder { id: "toy", dims: 4 };
    let journal = |name, middle| -> TestResult<Eja> {
        let eja = Eja::open(dir.path().join(name))?.with_clock(|| Timestamp(1));
        eja.notice(Source::Person, "相同开头")?;
        eja.notice(Source::Person, middle)?;
        eja.notice(Source::Person, "相同结尾")?;
        Ok(eja)
    };
    let first = journal("first.db", "喜欢喝茶")?;
    let second = journal("second.db", "钥匙在抽屉")?;
    assert_eq!(first.revision()?, second.revision()?);
    let mut memory = MemoryIndex::open(&path, encoder())?;
    assert!(memory.search(&first, "茶")?[0].similarity > 0.99);
    assert!(memory
        .search(&second, "茶")?
        .iter()
        .all(|hit| hit.similarity < 0.7));
    drop(memory);
    let mut reopened = MemoryIndex::open(&path, encoder())?;
    assert_eq!(reopened.search(&first, "茶")?[0].evidence.entry.id, 2);
    assert!(reopened.search(&first, "茶")?[0].similarity > 0.99);
    Ok(())
}
