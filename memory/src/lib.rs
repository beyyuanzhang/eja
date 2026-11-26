//! Derived full-history search for `eja-core` journals.
//! Consumes only `eja-core`'s public read APIs and keeps vector state in a separate cache DB.
#[cfg(feature = "onnx")]
mod onnx;
#[cfg(feature = "onnx")]
pub use onnx::OnnxEncoder;

use eja_core::{eja, Entry, Evidence, Fact, Id, Outcome, FACT_BYTES};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, path::Path, sync::OnceLock};

const BATCH: usize = 256;
const TOP: usize = 10;
const CHARS: usize = 384;
const STRIDE: usize = 320;
const DELIMITERS: &str = ",，、。;；:：|()（）[]\"'“”「」";

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Conflict,
    Invalid(String),
    Backend(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => f.write_str("memory index changed during search"),
            Self::Invalid(msg) | Self::Backend(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<eja_core::Error> for Error {
    fn from(err: eja_core::Error) -> Self {
        Self::Backend(err.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Self::Backend(err.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Backend(err.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Passage,
    Query,
}

/// Supplies a shared vector space for passages and queries.
/// The identity must change with weights, tokenizer, pooling, or input formatting.
pub trait Encoder: Send {
    fn identity(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn encode(
        &mut self,
        kind: InputKind,
        texts: &[&str],
    ) -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>>;
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
    #[serde(flatten)]
    pub evidence: Evidence,
    /// Vector similarity, or keyword hit ratio in keyword mode.
    pub similarity: f64,
}

pub struct MemoryIndex {
    semantic: Option<SemanticIndex>,
}

struct SemanticIndex {
    db: Connection,
    encoder: Box<dyn Encoder>,
    dimensions: usize,
    signature: String,
    verified: Option<(u64, Checkpoint)>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Checkpoint {
    through: Id,
    fingerprint: [u8; 32],
}

impl Checkpoint {
    fn advance(mut self, entries: &[Entry], through: Id) -> Result<Self> {
        let mut digest = Sha256::new();
        for entry in entries {
            digest.update(self.fingerprint);
            digest.update(serde_json::to_vec(entry)?);
            self.fingerprint = digest.finalize_reset().into();
        }
        self.through = through;
        Ok(self)
    }
}

impl MemoryIndex {
    /// Keyword-only search directly over `eja` public read APIs (zero disk cache, zero ML).
    pub fn keyword() -> Self {
        Self { semantic: None }
    }

    /// Open or attach a derived vector cache database for semantic retrieval.
    pub fn open(index_path: impl AsRef<Path>, encoder: impl Encoder + 'static) -> Result<Self> {
        register_vectors()?;
        let db = Connection::open(index_path.as_ref())?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        let semantic = SemanticIndex::attach(db, Box::new(encoder))?;
        Ok(Self {
            semantic: Some(semantic),
        })
    }

    /// Search all text-bearing history in `eja`.
    pub fn search(&mut self, eja: &eja, cue: &str) -> Result<Vec<Match>> {
        self.search_at(eja, cue, eja.revision()?)
    }

    /// Incrementally index up to one batch of new journal entries into the derived cache.
    /// Returns `Ok(true)` when a batch was processed and more entries may still remain.
    pub fn warm(&mut self, eja: &eja) -> Result<bool> {
        let Some(index) = self.semantic.as_mut() else {
            return Ok(false);
        };
        let head = eja.revision()?;
        if head <= 0 {
            return Ok(false);
        }
        index.catch_up_step(eja, head)
    }

    /// Rank observations at the given revision. Use keywords if vectors are unavailable.
    pub fn search_at(&mut self, eja: &eja, cue: &str, head: Id) -> Result<Vec<Match>> {
        if cue.trim().is_empty() || cue.len() > FACT_BYTES {
            return Err(Error::Invalid(
                "search requires a bounded non-empty cue".into(),
            ));
        }
        if head <= 0 {
            return Ok(Vec::new());
        }
        if let Some(hits) = self
            .semantic
            .as_mut()
            .and_then(|index| index.search(eja, cue, head).ok())
        {
            return Ok(hits);
        }
        keyword_search_at(eja, cue, head)
    }
}

fn register_vectors() -> Result<()> {
    static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let status = REGISTERED.get_or_init(|| {
        unsafe {
            rusqlite::auto_extension::register_auto_extension(std::mem::transmute::<
                unsafe extern "C" fn(),
                rusqlite::auto_extension::RawAutoExtension,
            >(
                sqlite_vec::sqlite3_vec_init
            ))
        }
        .map_err(|err| err.to_string())
    });
    status.clone().map_err(Error::Backend)
}

fn fact_text(fact: &Fact) -> Option<String> {
    match fact {
        Fact::Noticed { text, .. } | Fact::Said { text } | Fact::Kept { text, .. } => {
            Some(text.clone())
        }
        Fact::Acted { capability, input } => Some(format!("{capability} {input}")),
        Fact::Returned {
            outcome: Outcome::Observed(text) | Outcome::Failed(text) | Outcome::Unknown(text),
            ..
        } => Some(text.clone()),
        Fact::Released { .. } => None,
    }
}

fn tokenize_cue(cue: &str) -> Vec<String> {
    let mut words: Vec<String> = cue
        .split(|ch: char| ch.is_whitespace() || DELIMITERS.contains(ch))
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        words.push(cue.trim().to_lowercase());
    }
    words
}

fn lexical_ratio(text: &str, tokens: &[String]) -> f64 {
    if tokens.is_empty() {
        return 0.0;
    }
    let lower = text.to_lowercase();
    let hits = tokens
        .iter()
        .filter(|token| lower.contains(token.as_str()))
        .count();
    hits as f64 / tokens.len() as f64
}

fn fuse_scores(vector: f64, lexical: f64) -> f64 {
    let v = vector.clamp(0.0, 1.0);
    let l = lexical.clamp(0.0, 1.0);
    1.0 - (1.0 - v) * (1.0 - 0.5 * l)
}

fn rank_matches(eja: &eja, head: Id, mut scored: Vec<(Id, f64)>) -> Result<Vec<Match>> {
    scored.sort_unstable_by(|(lid, lscore), (rid, rscore)| {
        rscore.total_cmp(lscore).then(rid.cmp(lid))
    });
    scored
        .into_iter()
        .take(TOP)
        .map(|(id, similarity)| {
            Ok(Match {
                evidence: eja.evidence_at(id, head)?,
                similarity,
            })
        })
        .collect()
}

fn keyword_search_at(eja: &eja, cue: &str, head: Id) -> Result<Vec<Match>> {
    let words = tokenize_cue(cue);
    let mut scored = Vec::new();
    let mut cursor = 0;
    while cursor < head {
        let end = (cursor + BATCH as Id).min(head);
        for entry in eja.entries_after(cursor, end)? {
            let Some(text) = fact_text(&entry.fact) else {
                continue;
            };
            let ratio = lexical_ratio(&text, &words);
            if ratio > 0.0 {
                scored.push((entry.id, ratio));
            }
        }
        cursor = end;
    }
    rank_matches(eja, head, scored)
}

fn passages(text: &str) -> Vec<&str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let indices: Vec<usize> = trimmed.char_indices().map(|(idx, _)| idx).collect();
    if indices.len() <= CHARS {
        return vec![trimmed];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < indices.len() {
        let end = indices.get(start + CHARS).copied().unwrap_or(trimmed.len());
        chunks.push(&trimmed[indices[start]..end]);
        if start + CHARS >= indices.len() {
            break;
        }
        start += STRIDE;
    }
    chunks
}

fn vector_bytes(vector: Vec<f32>, dimensions: usize) -> Result<Vec<u8>> {
    if vector.len() != dimensions
        || !vector.iter().all(|value| value.is_finite())
        || vector.iter().all(|value| *value == 0.0)
    {
        return Err(Error::Invalid("invalid sentence embedding".into()));
    }
    Ok(vector.into_iter().flat_map(f32::to_le_bytes).collect())
}

impl SemanticIndex {
    fn attach(db: Connection, encoder: Box<dyn Encoder>) -> Result<Self> {
        let dimensions = encoder.dimensions();
        if encoder.identity().is_empty() || !(1..=8192).contains(&dimensions) {
            return Err(Error::Invalid(
                "encoder needs an identity and a supported vector dimension".into(),
            ));
        }
        let signature = format!("{dimensions}/{}", encoder.identity());
        let tx = Transaction::new_unchecked(&db, TransactionBehavior::Immediate)?;
        let has_checkpoint: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('memory_meta') WHERE name='fingerprint')",
            [],
            |row| row.get(0),
        )?;
        let stored: Option<String> = if has_checkpoint {
            tx.query_row("SELECT signature FROM memory_meta", [], |row| row.get(0))
                .optional()?
        } else {
            None
        };
        if stored.as_deref() != Some(signature.as_str()) {
            tx.execute_batch(
                "DROP TABLE IF EXISTS memory_vectors;
                 DROP TABLE IF EXISTS memory_refs;
                 DROP TABLE IF EXISTS memory_keys;
                 DROP TABLE IF EXISTS memory_meta;",
            )?;
        }
        tx.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS memory_meta(
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 signature TEXT NOT NULL,
                 through_id INTEGER NOT NULL,
                 fingerprint BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS memory_keys(
                 id INTEGER PRIMARY KEY,
                 passage TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS memory_refs(
                 key INTEGER NOT NULL,
                 fact INTEGER NOT NULL,
                 PRIMARY KEY(key, fact)
             ) WITHOUT ROWID;
             CREATE VIRTUAL TABLE IF NOT EXISTS memory_vectors
                 USING vec0(embedding float[{dimensions}] distance_metric=cosine, first_event integer);"
        ))?;
        tx.execute(
            "INSERT OR IGNORE INTO memory_meta VALUES(1, ?1, 0, zeroblob(32))",
            [&signature],
        )?;
        tx.commit()?;
        Ok(Self {
            db,
            encoder,
            dimensions,
            signature,
            verified: None,
        })
    }

    fn encode(&mut self, kind: InputKind, inputs: &[&str]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self
            .encoder
            .encode(kind, inputs)
            .map_err(|err| Error::Backend(err.to_string()))?;
        if vectors.len() != inputs.len() {
            return Err(Error::Invalid(
                "encoder returned a different number of vectors than inputs".into(),
            ));
        }
        Ok(vectors)
    }

    fn checkpoint(&self, conn: &Connection) -> Result<Checkpoint> {
        conn.query_row(
            "SELECT through_id, fingerprint FROM memory_meta WHERE singleton=1 AND signature=?1",
            [&self.signature],
            |row| {
                Ok(Checkpoint {
                    through: row.get(0)?,
                    fingerprint: row.get(1)?,
                })
            },
        )
        .optional()?
        .ok_or(Error::Conflict)
    }

    fn verify_prefix(&mut self, eja: &eja, through: Id) -> Result<Checkpoint> {
        let instance = eja.instance_id();
        let mut proof = self
            .verified
            .filter(|(id, checkpoint)| *id == instance && checkpoint.through <= through)
            .map(|(_, checkpoint)| checkpoint)
            .unwrap_or_default();
        while proof.through < through {
            let end = proof.through.saturating_add(BATCH as Id).min(through);
            proof = proof.advance(&eja.entries_after(proof.through, end)?, end)?;
        }
        self.verified = Some((instance, proof));
        Ok(proof)
    }

    fn catch_up_step(&mut self, eja: &eja, through: Id) -> Result<bool> {
        loop {
            let cached = self.checkpoint(&self.db)?;
            let cursor = cached.through;
            if (cursor > through && cursor > eja.revision()?)
                || self.verify_prefix(eja, cursor)? != cached
            {
                let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
                if self.checkpoint(&tx)? == cached {
                    tx.execute_batch(
                        "DELETE FROM memory_vectors;
                         DELETE FROM memory_refs;
                         DELETE FROM memory_keys;
                         UPDATE memory_meta SET through_id=0, fingerprint=zeroblob(32);",
                    )?;
                    tx.commit()?;
                }
                continue;
            }
            if cursor >= through {
                return Ok(false);
            }
            let batch_end = cursor.saturating_add(BATCH as Id).min(through);
            let entries = eja.entries_after(cursor, batch_end)?;
            let last = entries.last().map_or(batch_end, |entry| entry.id);
            let updated = cached.advance(&entries, last)?;
            let mut sources: BTreeMap<String, Vec<Id>> = BTreeMap::new();
            for entry in entries {
                if let Some(text) = fact_text(&entry.fact) {
                    for chunk in passages(&text) {
                        sources.entry(chunk.to_owned()).or_default().push(entry.id);
                    }
                }
            }
            let unknown_owned: Vec<String> = {
                let mut lookup = self
                    .db
                    .prepare_cached("SELECT id FROM memory_keys WHERE passage=?1")?;
                let mut missing = Vec::new();
                for chunk in sources.keys() {
                    if !lookup.exists([chunk])? {
                        missing.push(chunk.clone());
                    }
                }
                missing
            };
            let unknown_refs: Vec<&str> = unknown_owned.iter().map(String::as_str).collect();
            let vectors = self.encode(InputKind::Passage, &unknown_refs)?;

            let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            if self.checkpoint(&tx)? != cached {
                continue;
            }
            {
                let mut insert_key =
                    tx.prepare_cached("INSERT INTO memory_keys(passage) VALUES(?1) RETURNING id")?;
                let mut insert_vec = tx.prepare_cached(
                    "INSERT INTO memory_vectors(rowid,embedding,first_event) VALUES(?1,?2,?3)",
                )?;
                for (chunk, vector) in unknown_owned.iter().zip(vectors) {
                    let key: Id = insert_key.query_row([chunk], |row| row.get(0))?;
                    insert_vec.execute(params![
                        key,
                        vector_bytes(vector, self.dimensions)?,
                        sources[chunk][0]
                    ])?;
                }
                let mut lookup =
                    tx.prepare_cached("SELECT id FROM memory_keys WHERE passage=?1")?;
                let mut link =
                    tx.prepare_cached("INSERT OR IGNORE INTO memory_refs VALUES(?1,?2)")?;
                for (chunk, ids) in &sources {
                    let key: Id = lookup.query_row([chunk], |row| row.get(0))?;
                    for id in ids {
                        link.execute(params![key, id])?;
                    }
                }
            }
            tx.execute(
                "UPDATE memory_meta SET through_id=?1, fingerprint=?2 WHERE singleton=1",
                params![updated.through, updated.fingerprint],
            )?;
            tx.commit()?;
            self.verified = Some((eja.instance_id(), updated));
            return Ok(true);
        }
    }

    fn catch_up(&mut self, eja: &eja, through: Id) -> Result<Checkpoint> {
        while self.catch_up_step(eja, through)? {}
        self.checkpoint(&self.db)
    }

    fn search(&mut self, eja: &eja, cue: &str, head: Id) -> Result<Vec<Match>> {
        let verified = self.catch_up(eja, head)?;
        let chunks = passages(cue);
        let queries = self.encode(InputKind::Query, &chunks)?;
        let tokens = tokenize_cue(cue);
        let limit = (TOP * 32).min(256);
        let mut vector_scores: BTreeMap<Id, f64> = BTreeMap::new();
        let mut lexical_scores: BTreeMap<Id, f64> = BTreeMap::new();
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Deferred)?;
        if self.checkpoint(&tx)? != verified {
            return Err(Error::Conflict);
        }
        {
            let mut stmt = tx.prepare_cached(
                "SELECT rowid,distance FROM memory_vectors
                 WHERE embedding MATCH ?1 AND k=?2 AND first_event<=?3 ORDER BY distance",
            )?;
            let mut origin = tx.prepare_cached(
                "SELECT fact FROM memory_refs WHERE key=?1 AND fact<=?2 ORDER BY fact DESC LIMIT 1",
            )?;
            for query in queries {
                let rows = stmt.query_map(
                    params![vector_bytes(query, self.dimensions)?, limit, head],
                    |row| Ok((row.get::<_, Id>(0)?, row.get::<_, f64>(1)?)),
                )?;
                for row in rows {
                    let (key, distance) = row?;
                    let score = 1.0 - distance;
                    let id: Id = origin.query_row(params![key, head], |row| row.get(0))?;
                    vector_scores
                        .entry(id)
                        .and_modify(|best| *best = best.max(score))
                        .or_insert(score);
                }
            }
            let mut scan = tx.prepare_cached(
                "SELECT k.id, k.passage FROM memory_keys k
                 WHERE EXISTS(SELECT 1 FROM memory_refs r WHERE r.key=k.id AND r.fact<=?1)",
            )?;
            let rows = scan.query_map([head], |row| {
                Ok((row.get::<_, Id>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (key, passage) = row?;
                let ratio = lexical_ratio(&passage, &tokens);
                if ratio > 0.0 {
                    let id: Id = origin.query_row(params![key, head], |row| row.get(0))?;
                    lexical_scores
                        .entry(id)
                        .and_modify(|best| *best = best.max(ratio))
                        .or_insert(ratio);
                }
            }
        }
        drop(tx);
        let mut combined: BTreeMap<Id, f64> = BTreeMap::new();
        for (id, vec_score) in vector_scores {
            let lex = lexical_scores.remove(&id).unwrap_or(0.0);
            combined.insert(id, fuse_scores(vec_score, lex));
        }
        for (id, lex) in lexical_scores {
            combined.insert(id, fuse_scores(0.0, lex));
        }
        rank_matches(eja, head, combined.into_iter().collect())
    }
}
