//! Derived dense passage index over the subject's journal. Only returned matches are bounded.
use crate::{Eja, Encoder, Error, Evidence, Fact, Id, InputKind, Outcome, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use std::{cell::RefCell, collections::BTreeMap, sync::OnceLock};

const BATCH: usize = 256;
const TOP: usize = 4;
const CHARS: usize = 384;
const STRIDE: usize = 320;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
    #[serde(flatten)]
    pub evidence: Evidence,
    pub similarity: f64,
}

pub(super) struct Memory {
    encoder: RefCell<Box<dyn Encoder>>,
    dimensions: usize,
}

pub(super) fn register_vectors() -> Result<()> {
    static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let status = REGISTERED.get_or_init(|| {
        // sqlite-vec exports SQLite's C extension initializer with an erased signature.
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
    status.clone().map_err(Error::Storage)
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
        let end_char = (start + CHARS).min(indices.len());
        let start_byte = indices[start];
        let end_byte = if end_char == indices.len() {
            trimmed.len()
        } else {
            indices[end_char]
        };
        chunks.push(&trimmed[start_byte..end_byte]);
        if end_char == indices.len() {
            break;
        }
        start += STRIDE;
    }
    chunks
}

fn content(fact: Fact) -> Option<String> {
    match fact {
        Fact::Noticed { text, .. } => Some(text),
        Fact::Kept { text, .. } => Some(text),
        Fact::Acted { capability, input } => Some(format!("{capability} {input}")),
        Fact::Returned {
            outcome: Outcome::Observed(text) | Outcome::Failed(text) | Outcome::Unknown(text),
            ..
        } => Some(text),
        Fact::Released { .. } => None,
    }
}

fn bytes(vector: Vec<f32>, dimensions: usize) -> Result<Vec<u8>> {
    if vector.len() != dimensions
        || !vector.iter().all(|value| value.is_finite())
        || vector.iter().all(|value| *value == 0.0)
    {
        return Err(Error::invalid("invalid sentence embedding"));
    }
    Ok(vector.into_iter().flat_map(f32::to_le_bytes).collect())
}

impl Eja {
    /// Search all text-bearing history, including events never marked as concerns.
    pub fn search(&self, cue: &str) -> Result<Vec<Match>> {
        let memory = self
            .1
            .as_ref()
            .ok_or_else(|| Error::invalid("no semantic encoder configured"))?;
        let head = self.head()?;
        memory.search(self, cue, head, head)
    }
}

impl Memory {
    pub(super) fn attach(db: &Connection, encoder: Box<dyn Encoder>) -> Result<Self> {
        let dimensions = encoder.dimensions();
        if encoder.identity().is_empty() || !(1..=8192).contains(&dimensions) {
            return Err(Error::invalid(
                "encoder needs an identity and a supported vector dimension",
            ));
        }
        let signature = format!("{dimensions}/{}", encoder.identity());
        let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_meta(
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 signature TEXT NOT NULL,
                 through_id INTEGER NOT NULL
             )",
        )?;
        let stored: Option<String> = tx
            .query_row("SELECT signature FROM memory_meta", [], |row| row.get(0))
            .optional()?;
        if stored
            .as_deref()
            .is_some_and(|existing| existing != signature)
        {
            tx.execute_batch(
                "DROP TABLE IF EXISTS memory_vectors;
                 DROP TABLE IF EXISTS memory_refs;
                 DROP TABLE IF EXISTS memory_keys;
                 DELETE FROM memory_meta;",
            )?;
        }
        tx.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS memory_keys(
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
            "INSERT OR IGNORE INTO memory_meta VALUES(1, ?1, 0)",
            [&signature],
        )?;
        tx.commit()?;
        Ok(Self {
            encoder: RefCell::new(encoder),
            dimensions,
        })
    }

    fn encode(&self, kind: InputKind, inputs: &[&str]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self
            .encoder
            .borrow_mut()
            .encode(kind, inputs)
            .map_err(Error::External)?;
        if vectors.len() != inputs.len() {
            return Err(Error::invalid(
                "encoder returned a different number of vectors than inputs",
            ));
        }
        Ok(vectors)
    }

    fn catch_up(&self, db: &Connection, through: Id) -> Result<()> {
        loop {
            let cursor: Id =
                db.query_row("SELECT through_id FROM memory_meta", [], |row| row.get(0))?;
            if cursor >= through {
                return Ok(());
            }
            let mut sources: BTreeMap<String, Vec<Id>> = BTreeMap::new();
            let mut last = cursor;
            let mut stmt = db
                .prepare("SELECT id,body FROM facts WHERE id>?1 AND id<=?2 ORDER BY id LIMIT ?3")?;
            let rows = stmt.query_map(params![cursor, through, BATCH], |row| {
                Ok((row.get::<_, Id>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, body) = row?;
                last = id;
                if let Some(text) = content(serde_json::from_str(&body)?) {
                    for chunk in passages(&text) {
                        sources.entry(chunk.to_owned()).or_default().push(id);
                    }
                }
            }
            drop(stmt);
            if last == cursor {
                return Err(Error::Storage("memory cursor cannot advance".into()));
            }
            let mut unknown = Vec::new();
            let mut lookup = db.prepare_cached("SELECT id FROM memory_keys WHERE passage=?1")?;
            for chunk in sources.keys() {
                if !lookup.exists([chunk])? {
                    unknown.push(chunk.as_str());
                }
            }
            let vectors = self.encode(InputKind::Passage, &unknown)?;
            // Encoding is outside the write transaction. A racing indexer can win this batch.
            let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
            if tx.execute(
                "UPDATE memory_meta SET through_id=?1 WHERE through_id=?2",
                params![last, cursor],
            )? == 0
            {
                continue;
            }
            let mut insert_key =
                tx.prepare_cached("INSERT INTO memory_keys(passage) VALUES(?1) RETURNING id")?;
            let mut insert_vec = tx.prepare_cached(
                "INSERT INTO memory_vectors(rowid,embedding,first_event) VALUES(?1,?2,?3)",
            )?;
            for (chunk, vector) in unknown.into_iter().zip(vectors) {
                let key: Id = insert_key.query_row([chunk], |row| row.get(0))?;
                insert_vec.execute(params![
                    key,
                    bytes(vector, self.dimensions)?,
                    sources[chunk][0]
                ])?;
            }
            drop((insert_key, insert_vec));
            let mut link = tx.prepare_cached("INSERT OR IGNORE INTO memory_refs VALUES(?1,?2)")?;
            for (chunk, ids) in &sources {
                let key: Id = lookup.query_row([chunk], |row| row.get(0))?;
                for id in ids {
                    link.execute(params![key, id])?;
                }
            }
            drop(link);
            tx.commit()?;
        }
    }

    pub(super) fn search(
        &self,
        eja: &Eja,
        cue: &str,
        through: Id,
        revision: Id,
    ) -> Result<Vec<Match>> {
        if through <= 0 || cue.trim().is_empty() {
            return Ok(Vec::new());
        }
        if cue.len() > crate::FACT_BYTES {
            return Err(Error::invalid("memory query exceeds fact budget"));
        }
        self.catch_up(&eja.0, through)?;
        let chunks = passages(cue);
        let queries = self.encode(InputKind::Query, &chunks)?;
        // One fact fits in at most this many chunks. Collect enough keys for TOP distinct facts.
        let limit = TOP * (crate::FACT_BYTES / STRIDE + 2);
        let mut scores: BTreeMap<Id, f64> = BTreeMap::new();
        let mut stmt = eja.0.prepare_cached(
            "SELECT rowid,distance FROM memory_vectors
             WHERE embedding MATCH ?1 AND k=?2 AND first_event<=?3 ORDER BY distance",
        )?;
        let mut origin = eja.0.prepare_cached(
            "SELECT fact FROM memory_refs WHERE key=?1 AND fact<=?2 ORDER BY fact DESC LIMIT 1",
        )?;
        for query in queries {
            let rows = stmt.query_map(
                params![bytes(query, self.dimensions)?, limit, through],
                |row| Ok((row.get::<_, Id>(0)?, row.get::<_, f64>(1)?)),
            )?;
            for row in rows {
                let (key, distance) = row?;
                let score = 1.0 - distance;
                let id: Id = origin.query_row(params![key, through], |row| row.get(0))?;
                scores
                    .entry(id)
                    .and_modify(|best| *best = best.max(score))
                    .or_insert(score);
            }
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_unstable_by(|(left_id, left_score), (right_id, right_score)| {
            right_score
                .total_cmp(left_score)
                .then(right_id.cmp(left_id))
        });
        ranked
            .into_iter()
            .take(TOP)
            .map(|(id, similarity)| {
                Ok(Match {
                    evidence: eja.evidence(id, revision)?,
                    similarity,
                })
            })
            .collect()
    }
}
