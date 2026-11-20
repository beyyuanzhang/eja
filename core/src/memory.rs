//! Dense passage retrieval over the subject's journal. Only returned matches are bounded.
use crate::{Choice, Eja, Encoder, Entry, Fact, Id, InputKind, Outcome, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeMap};

const BATCH: usize = 256;
const TOP: usize = 4;
const CHARS: usize = 384;

#[derive(Debug, Serialize)]
pub struct Match {
    pub entry: Entry,
    pub similarity: f64,
    /// The current edit or release if this match is a superseded keep judgment.
    pub revision_of_concern: Option<Entry>,
}

pub(super) struct Memory {
    encoder: RefCell<Box<dyn Encoder>>,
    dimensions: usize,
    table_prefix: String,
}

pub(super) fn register_vectors() -> Result<()> {
    // sqlite-vec exports SQLite's C extension initializer with an erased signature.
    unsafe {
        rusqlite::auto_extension::register_auto_extension(std::mem::transmute::<
            unsafe extern "C" fn(),
            rusqlite::auto_extension::RawAutoExtension,
        >(sqlite_vec::sqlite3_vec_init))?;
    }
    Ok(())
}

fn passages(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text.trim();
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .char_indices()
            .nth(CHARS)
            .map_or(rest.len(), |(i, _)| i);
        let (part, tail) = rest.split_at(end);
        rest = tail;
        Some(part)
    })
}

fn content(fact: Fact) -> Option<String> {
    match fact {
        Fact::Noticed { text, .. } => Some(text),
        Fact::Judged { judgment } => match judgment.choice {
            Choice::Keep { text, .. } => Some(text),
            Choice::Act { capability, input } => Some(format!("{capability} {input}")),
            _ => None,
        },
        Fact::Returned {
            outcome: Outcome::Observed(s) | Outcome::Failed(s) | Outcome::Unknown(s),
            ..
        } => Some(s),
    }
}

fn bytes(vector: Vec<f32>, dimensions: usize) -> Result<Vec<u8>> {
    if vector.len() != dimensions
        || !vector.iter().all(|v| v.is_finite())
        || vector.iter().all(|v| *v == 0.)
    {
        return Err("invalid sentence embedding".into());
    }
    Ok(vector.into_iter().flat_map(f32::to_le_bytes).collect())
}

impl Eja {
    /// Search all text-bearing history, including events never marked as concerns.
    pub fn search(&self, cue: &str) -> Result<Vec<Match>> {
        let m = self.1.as_ref().ok_or("no semantic encoder configured")?;
        let head = self.head()?;
        m.search(self, cue, head, head)
    }
}

impl Memory {
    // Interpolate only the core-generated namespace; encoder identities are bound as data.
    fn sql(&self, statement: &str) -> String {
        statement.replace("memory_", &self.table_prefix)
    }

    pub(super) fn attach(db: &Connection, encoder: Box<dyn Encoder>) -> Result<Self> {
        let dimensions = encoder.dimensions();
        if encoder.identity().is_empty() || !(1..=8192).contains(&dimensions) {
            return Err("encoder needs an identity and a supported vector dimension".into());
        }
        let identity = encoder.identity().to_owned();
        let signature = format!("{dimensions}/{identity}");
        let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS memory_progress(singleton INTEGER PRIMARY KEY CHECK(singleton=1), model TEXT NOT NULL, through_id INTEGER NOT NULL)")?;
        let primary: Option<String> = tx
            .query_row("SELECT model FROM memory_progress", [], |r| r.get(0))
            .optional()?;
        // The first encoder keeps the original table names, including existing prototype indexes.
        let legacy = if primary.as_deref() == Some(&identity) && !identity.contains('/') {
            let schema: String = tx.query_row(
                "SELECT sql FROM sqlite_master WHERE name='memory_vectors'",
                [],
                |r| r.get(0),
            )?;
            schema.contains(&format!("float[{dimensions}]"))
        } else {
            false
        };
        let table_prefix = match primary {
            Some(stored) if stored != signature && !legacy => {
                format!("memory_{:x}_", Sha256::digest(signature.as_bytes()))
            }
            _ => "memory_".to_owned(),
        };
        let memory = Self {
            encoder: RefCell::new(encoder),
            dimensions,
            table_prefix,
        };
        tx.execute_batch(&memory.sql(&format!("CREATE TABLE IF NOT EXISTS memory_progress(singleton INTEGER PRIMARY KEY CHECK(singleton=1), model TEXT NOT NULL, through_id INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS memory_keys(id INTEGER PRIMARY KEY, passage TEXT NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS memory_refs(key INTEGER NOT NULL, fact INTEGER NOT NULL, PRIMARY KEY(key,fact)) WITHOUT ROWID;
            CREATE VIRTUAL TABLE IF NOT EXISTS memory_vectors USING vec0(embedding float[{dimensions}] distance_metric=cosine, first_event integer);")))?;
        tx.execute(
            &memory.sql("INSERT OR IGNORE INTO memory_progress VALUES(1,?1,0)"),
            [&signature],
        )?;
        tx.commit()?;
        Ok(memory)
    }

    fn encode(&self, kind: InputKind, inputs: &[&str]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.encoder.borrow_mut().encode(kind, inputs)?;
        if vectors.len() != inputs.len() {
            return Err("encoder returned a different number of vectors than inputs".into());
        }
        Ok(vectors)
    }

    fn catch_up(&self, db: &Connection, through: Id) -> Result<()> {
        loop {
            let cursor: Id = db.query_row(
                &self.sql("SELECT through_id FROM memory_progress"),
                [],
                |r| r.get(0),
            )?;
            if cursor >= through {
                return Ok(());
            }
            let mut sources: BTreeMap<String, Vec<Id>> = BTreeMap::new();
            let mut last = cursor;
            let mut stmt = db
                .prepare("SELECT id,body FROM facts WHERE id>?1 AND id<=?2 ORDER BY id LIMIT ?3")?;
            let rows = stmt.query_map(params![cursor, through, BATCH], |r| {
                Ok((r.get::<_, Id>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, body) = row?;
                last = id;
                if let Some(text) = content(serde_json::from_str(&body)?) {
                    for p in passages(&text) {
                        sources.entry(p.to_owned()).or_default().push(id);
                    }
                }
            }
            drop(stmt);
            if last == cursor {
                return Err("memory cursor cannot advance".into());
            }
            let mut unknown = Vec::new();
            let mut lookup =
                db.prepare_cached(&self.sql("SELECT id FROM memory_keys WHERE passage=?1"))?;
            for p in sources.keys() {
                if !lookup.exists([p])? {
                    unknown.push(p.as_str());
                }
            }
            let vectors = self.encode(InputKind::Passage, &unknown)?;
            // Encoding is outside the write transaction. A racing indexer can win this batch.
            let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
            if tx.execute(
                &self.sql("UPDATE memory_progress SET through_id=?1 WHERE through_id=?2"),
                params![last, cursor],
            )? == 0
            {
                continue;
            }
            let mut insert_key = tx.prepare_cached(
                &self.sql("INSERT INTO memory_keys(passage) VALUES(?1) RETURNING id"),
            )?;
            let mut insert_vec =
                tx.prepare_cached(&self.sql(
                    "INSERT INTO memory_vectors(rowid,embedding,first_event) VALUES(?1,?2,?3)",
                ))?;
            for (p, vec) in unknown.into_iter().zip(vectors) {
                let key: Id = insert_key.query_row([p], |r| r.get(0))?;
                insert_vec.execute(params![key, bytes(vec, self.dimensions)?, sources[p][0]])?;
            }
            drop((insert_key, insert_vec));
            let mut link =
                tx.prepare_cached(&self.sql("INSERT OR IGNORE INTO memory_refs VALUES(?1,?2)"))?;
            for (p, ids) in &sources {
                let key: Id = lookup.query_row([p], |r| r.get(0))?;
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
        if cue.trim().is_empty() {
            return Ok(vec![]);
        }
        if cue.len() > crate::FACT_BYTES {
            return Err("memory query exceeds fact budget".into());
        }
        self.catch_up(&eja.0, through)?;
        let chunks: Vec<_> = passages(cue).collect();
        let queries = self.encode(InputKind::Query, &chunks)?;
        // One fact fits in at most this many chunks. Collect enough keys for TOP distinct facts.
        let k = TOP * (crate::FACT_BYTES / CHARS + 2);
        let mut scores: BTreeMap<Id, f64> = BTreeMap::new();
        let mut stmt = eja.0.prepare_cached(&self.sql(
            "SELECT rowid,distance FROM memory_vectors
                WHERE embedding MATCH ?1 AND k=?2 AND first_event<=?3 ORDER BY distance",
        ))?;
        let mut origin = eja.0.prepare_cached(&self.sql(
            "SELECT fact FROM memory_refs WHERE key=?1 AND fact<=?2 ORDER BY fact DESC LIMIT 1",
        ))?;
        for query in queries {
            let rows = stmt
                .query_map(params![bytes(query, self.dimensions)?, k, through], |r| {
                    Ok((r.get::<_, Id>(0)?, r.get::<_, f64>(1)?))
                })?;
            for row in rows {
                let (key, distance) = row?;
                let id = origin.query_row(params![key, through], |r| r.get(0))?;
                let score = 1. - distance;
                scores
                    .entry(id)
                    .and_modify(|s| *s = s.max(score))
                    .or_insert(score);
            }
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_unstable_by(|(a, x), (b, y)| y.total_cmp(x).then(b.cmp(a)));
        ranked
            .into_iter()
            .take(TOP)
            .map(|(id, similarity)| {
                let newer: Option<Id> = eja
                    .0
                    .query_row(
                        "SELECT id FROM facts WHERE concern=(SELECT concern FROM facts WHERE id=?1)
                    AND id>?1 AND id<=?2 ORDER BY id DESC LIMIT 1",
                        params![id, revision],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(Match {
                    entry: eja.read(id)?,
                    similarity,
                    revision_of_concern: newer.map(|id| eja.read(id)).transpose()?,
                })
            })
            .collect()
    }
}
