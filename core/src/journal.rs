use crate::{
    Call, Concern, Consolidation, Eja, Entry, Error, Evidence, Fact, Id, Outcome, Result, Step,
    Timestamp, FACT_BYTES, PAGE, REPLAY_PAGE,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

fn head_on(conn: &Connection) -> Result<Id> {
    Ok(
        conn.query_row("SELECT coalesce(max(id), 0) FROM facts", [], |row| {
            row.get(0)
        })?,
    )
}

fn read_on(conn: &Connection, id: Id) -> Result<Entry> {
    let row: Option<(i64, String)> = conn
        .query_row("SELECT at, body FROM facts WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    let Some((at, body)) = row else {
        return Err(Error::invalid("unknown fact id"));
    };
    Ok(Entry {
        id,
        at: Timestamp(at),
        fact: serde_json::from_str(&body)?,
    })
}

fn require_open_on(conn: &Connection, concern: Id) -> Result<()> {
    let live = conn
        .query_row(
            "SELECT live FROM facts WHERE concern = ?1 ORDER BY id DESC LIMIT 1",
            [concern],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    if live == Some(true) {
        Ok(())
    } else {
        Err(Error::invalid("unknown or released concern"))
    }
}

fn receipt_at_on(conn: &Connection, action: Id, through: Id) -> Result<Option<Entry>> {
    let latest = conn
        .query_row(
            "SELECT id FROM facts WHERE json_extract(body,'$.kind')='returned'
             AND json_extract(body,'$.action')=?1 AND id<=?2 ORDER BY id DESC LIMIT 1",
            params![action, through],
            |row| row.get(0),
        )
        .optional()?;
    latest.map(|id| read_on(conn, id)).transpose()
}

fn consolidated_at_on(conn: &Connection, through: Id) -> Result<Option<Entry>> {
    let latest = conn
        .query_row(
            "SELECT id FROM facts WHERE json_extract(body,'$.kind')='consolidated'
             AND id<=?1 ORDER BY id DESC LIMIT 1",
            [through],
            |row| row.get(0),
        )
        .optional()?;
    latest.map(|id| read_on(conn, id)).transpose()
}

fn insert_fact(
    conn: &Connection,
    at: Timestamp,
    fact: &Fact,
    edit: Option<(Id, bool)>,
) -> Result<Id> {
    let body = serde_json::to_string(fact)?;
    if body.len() > FACT_BYTES {
        return Err(Error::invalid("fact exceeds the byte budget"));
    }
    let (concern, live) = edit.unzip();
    let id = conn.query_row(
        "INSERT INTO facts(at, body, concern, live) VALUES(?1, ?2, ?3, ?4) RETURNING id",
        params![at.as_unix(), body, concern, live],
        |row| row.get(0),
    )?;
    Ok(id)
}

impl Eja {
    pub(super) fn initialize(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS facts (
                 id INTEGER PRIMARY KEY,
                 at INTEGER NOT NULL,
                 body TEXT NOT NULL,
                 concern INTEGER,
                 live INTEGER
             );
             CREATE INDEX IF NOT EXISTS keep_events ON facts(id) WHERE live=1;
             CREATE INDEX IF NOT EXISTS concern_edits ON facts(concern, id DESC);
             CREATE INDEX IF NOT EXISTS action_receipts ON facts(json_extract(body,'$.action'), id)
                 WHERE json_extract(body,'$.kind')='returned';
             CREATE INDEX IF NOT EXISTS consolidations ON facts(id DESC)
                 WHERE json_extract(body,'$.kind')='consolidated';",
        )?;
        Ok(Self {
            db,
            memory: None,
            clock: Box::new(Timestamp::now),
        })
    }

    pub(super) fn head(&self) -> Result<Id> {
        head_on(&self.db)
    }

    pub(super) fn now_at(&self, conn: &Connection) -> Result<Timestamp> {
        let last: Option<i64> = conn
            .query_row("SELECT at FROM facts ORDER BY id DESC LIMIT 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let now = self.clock_now();
        Ok(match last {
            Some(prev) => now.max(Timestamp(prev)),
            None => now,
        })
    }

    pub(super) fn append_notice(&self, fact: &Fact) -> Result<Id> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let at = self.now_at(&tx)?;
        let id = insert_fact(&tx, at, fact, None)?;
        tx.commit()?;
        Ok(id)
    }

    /// All invariant checks and writes for one step run inside one IMMEDIATE SQLite transaction.
    pub(super) fn commit_step(
        &self,
        revision: Id,
        step: &Step,
        grants: &[&str],
    ) -> Result<Vec<Call>> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if head_on(&tx)? != revision {
            return Err(Error::Conflict);
        }
        let at = self.now_at(&tx)?;
        let mut next = revision;
        for item in &step.keep {
            if item.text.trim().is_empty() {
                return Err(Error::invalid("a concern needs text"));
            }
            next = next
                .checked_add(1)
                .ok_or_else(|| Error::Storage("journal id exhausted".into()))?;
            let concern = match item.concern {
                Some(id) => {
                    require_open_on(&tx, id)?;
                    id
                }
                None => next,
            };
            let id = insert_fact(
                &tx,
                at,
                &Fact::Kept {
                    concern,
                    text: item.text.clone(),
                },
                Some((concern, true)),
            )?;
            debug_assert_eq!(id, next);
        }
        for &concern in &step.release {
            require_open_on(&tx, concern)?;
            next = next
                .checked_add(1)
                .ok_or_else(|| Error::Storage("journal id exhausted".into()))?;
            let id = insert_fact(&tx, at, &Fact::Released { concern }, Some((concern, false)))?;
            debug_assert_eq!(id, next);
        }
        let mut calls = Vec::with_capacity(step.act.len());
        for item in &step.act {
            if item.capability.is_empty() || !grants.contains(&item.capability.as_str()) {
                return Err(Error::invalid("capability is not granted"));
            }
            next = next
                .checked_add(1)
                .ok_or_else(|| Error::Storage("journal id exhausted".into()))?;
            let id = insert_fact(
                &tx,
                at,
                &Fact::Acted {
                    capability: item.capability.clone(),
                    input: item.input.clone(),
                },
                None,
            )?;
            debug_assert_eq!(id, next);
            calls.push(Call {
                id,
                capability: item.capability.clone(),
                input: item.input.clone(),
            });
        }
        tx.commit()?;
        Ok(calls)
    }

    pub fn consolidated(&self, revision: Id) -> Result<Option<Entry>> {
        consolidated_at_on(&self.db, revision)
    }

    /// Pure read-only next chronological batch of unconsolidated non-consolidation facts.
    pub fn unconsolidated(&self) -> Result<Option<Consolidation>> {
        let revision = self.head()?;
        if revision == 0 {
            return Ok(None);
        }
        let previous = self.consolidated(revision)?;
        let floor = match previous.as_ref().map(|entry| &entry.fact) {
            Some(Fact::Consolidated { through, .. }) => *through,
            _ => 0,
        };
        let mut stmt = self.db.prepare(
            "SELECT id, at, body FROM facts
             WHERE id > ?1 AND id <= ?2 AND json_extract(body,'$.kind') != 'consolidated'
             ORDER BY id ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![floor, revision, REPLAY_PAGE], |row| {
            Ok((
                row.get::<_, Id>(0)?,
                Timestamp(row.get::<_, i64>(1)?),
                row.get::<_, String>(2)?,
            ))
        })?;
        let entries: Vec<Entry> = rows
            .map(|row| {
                let (id, at, body) = row?;
                Ok(Entry {
                    id,
                    at,
                    fact: serde_json::from_str(&body)?,
                })
            })
            .collect::<Result<_>>()?;
        let Some(last) = entries.last() else {
            return Ok(None);
        };
        Ok(Some(Consolidation {
            revision,
            now: self.now_at(&self.db)?,
            concerns: self.concerns(revision, revision)?,
            previous,
            through: last.id,
            entries,
        }))
    }

    /// Atomically commit one consolidated memory covering `(prev_through, through]` against `revision`.
    pub fn commit_consolidation(
        &self,
        revision: Id,
        through: Id,
        text: impl Into<String>,
    ) -> Result<Id> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(Error::invalid("consolidation needs text"));
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if head_on(&tx)? != revision {
            return Err(Error::Conflict);
        }
        let floor = match consolidated_at_on(&tx, revision)?.map(|entry| entry.fact) {
            Some(Fact::Consolidated { through: prev, .. }) => prev,
            _ => 0,
        };
        if through <= floor || through > revision {
            return Err(Error::invalid(
                "consolidation through must advance within the committed view",
            ));
        }
        let has_new: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM facts
                 WHERE id > ?1 AND id <= ?2 AND json_extract(body,'$.kind') != 'consolidated'
             )",
            params![floor, through],
            |row| row.get(0),
        )?;
        if !has_new {
            return Err(Error::invalid(
                "consolidation range must include at least one unconsolidated fact",
            ));
        }
        let at = self.now_at(&tx)?;
        let id = insert_fact(&tx, at, &Fact::Consolidated { through, text }, None)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn evidence(&self, id: Id, revision: Id) -> Result<Evidence> {
        let entry = self.read(id)?;
        let latest = match &entry.fact {
            Fact::Returned { action, .. } => receipt_at_on(&self.db, *action, revision)?,
            Fact::Acted { .. } => receipt_at_on(&self.db, id, revision)?,
            Fact::Kept { concern, .. } => {
                let later = self
                    .db
                    .query_row(
                        "SELECT id FROM facts WHERE concern=?1 AND id>?2 AND id<=?3 ORDER BY id DESC LIMIT 1",
                        params![concern, id, revision],
                        |row| row.get(0),
                    )
                    .optional()?;
                later.map(|later_id| self.read(later_id)).transpose()?
            }
            Fact::Consolidated { .. } => consolidated_at_on(&self.db, revision)?,
            Fact::Noticed { .. } | Fact::Released { .. } => None,
        };
        Ok(Evidence {
            latest: latest.filter(|later| later.id > id),
            entry,
        })
    }

    /// An unknown receipt can be settled once. Retrying a recorded receipt returns its ID.
    pub fn finish(&self, action: Id, outcome: Outcome) -> Result<Id> {
        let mut fact = Fact::Returned { action, outcome };
        if serde_json::to_vec(&fact)?.len() > FACT_BYTES {
            let Fact::Returned {
                outcome: Outcome::Observed(text) | Outcome::Failed(text) | Outcome::Unknown(text),
                ..
            } = &mut fact
            else {
                unreachable!()
            };
            let suffix = format!(
                "… [truncated; {} bytes; sha256:{:x}]",
                text.len(),
                Sha256::digest(text.as_bytes())
            );
            // A byte can expand to six bytes when escaped in JSON.
            text.truncate(text.floor_char_boundary((FACT_BYTES - 256) / 6));
            text.push_str(&suffix);
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if !matches!(read_on(&tx, action)?.fact, Fact::Acted { .. }) {
            return Err(Error::invalid("outcome needs an action id"));
        }
        // At most two immutable receipts: the first report and its definitive resolution.
        let encoded = serde_json::to_string(&fact)?;
        let existing: Option<Id> = tx
            .query_row(
                "SELECT id FROM facts WHERE json_extract(body,'$.kind')='returned'
                 AND json_extract(body,'$.action')=?1 AND body=?2",
                params![action, encoded],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let previous = receipt_at_on(&tx, action, Id::MAX)?;
        if let Some(entry) = &previous {
            if !matches!(
                entry.fact,
                Fact::Returned {
                    outcome: Outcome::Unknown(_),
                    ..
                }
            ) || matches!(
                fact,
                Fact::Returned {
                    outcome: Outcome::Unknown(_),
                    ..
                }
            ) {
                return Err(Error::invalid("action already has a different outcome"));
            }
        }
        let at = self.now_at(&tx)?;
        let id = insert_fact(&tx, at, &fact, None)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn read(&self, id: Id) -> Result<Entry> {
        read_on(&self.db, id)
    }

    /// Inclusive cursor. Continue at the last returned id minus one.
    pub fn history(&self, through: Id) -> Result<Vec<Entry>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, at, body FROM facts WHERE id <= ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![through, PAGE], |row| {
            Ok((
                row.get::<_, Id>(0)?,
                Timestamp(row.get::<_, i64>(1)?),
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, at, body) = row?;
            Ok(Entry {
                id,
                at,
                fact: serde_json::from_str(&body)?,
            })
        })
        .collect()
    }

    /// Concerns as of a fixed view. Page with the last returned latest id minus one.
    pub fn concerns(&self, revision: Id, through: Id) -> Result<Vec<Concern>> {
        let mut stmt = self.db.prepare(
            "SELECT f.concern, f.id, f.at, json_extract(f.body, '$.text')
             FROM facts f WHERE f.live = 1 AND f.id <= min(?1, ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM facts later WHERE later.concern = f.concern
                       AND later.id > f.id AND later.id <= ?1
               )
             ORDER BY f.id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![revision, through, PAGE], |row| {
            Ok(Concern {
                id: row.get(0)?,
                latest: row.get(1)?,
                at: Timestamp(row.get(2)?),
                text: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
