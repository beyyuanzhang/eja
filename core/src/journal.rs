use crate::{
    Call, Concern, Eja, Entry, Error, Evidence, Fact, Id, Outcome, Result, Timestamp, FACT_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i32 = 1;

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

fn read_entry_row(row: &Row<'_>) -> rusqlite::Result<(Id, Timestamp, String)> {
    Ok((
        row.get::<_, Id>(0)?,
        Timestamp(row.get::<_, i64>(1)?),
        row.get::<_, String>(2)?,
    ))
}

fn decode_entries(
    rows: impl Iterator<Item = rusqlite::Result<(Id, Timestamp, String)>>,
) -> Result<Vec<Entry>> {
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
             PRAGMA busy_timeout = 5000;",
        )?;
        migrate_schema(&db)?;
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

    pub(super) fn append_fact(&self, fact: &Fact) -> Result<Id> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let at = self.now_at(&tx)?;
        let id = insert_fact(&tx, at, fact, None)?;
        tx.commit()?;
        Ok(id)
    }

    /// Open a new concern (`concern: None`) or revise an existing open concern (`concern: Some(id)`).
    pub fn keep(&self, concern: Option<Id>, text: impl Into<String>) -> Result<Entry> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(Error::invalid("a concern needs text"));
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let at = self.now_at(&tx)?;
        let concern = match concern {
            Some(id) => {
                require_open_on(&tx, id)?;
                id
            }
            None => head_on(&tx)?
                .checked_add(1)
                .ok_or_else(|| Error::Storage("journal id exhausted".into()))?,
        };
        let fact = Fact::Kept { concern, text };
        let id = insert_fact(&tx, at, &fact, Some((concern, true)))?;
        tx.commit()?;
        Ok(Entry { id, at, fact })
    }

    /// Release an open concern.
    pub fn release(&self, concern: Id) -> Result<Entry> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        require_open_on(&tx, concern)?;
        let at = self.now_at(&tx)?;
        let fact = Fact::Released { concern };
        let id = insert_fact(&tx, at, &fact, Some((concern, false)))?;
        tx.commit()?;
        Ok(Entry { id, at, fact })
    }

    /// Atomically record one authorized external action against an exact revision.
    pub fn act(
        &self,
        revision: Id,
        capability: impl Into<String>,
        input: Value,
        grants: &[&str],
    ) -> Result<Call> {
        let capability = capability.into();
        if capability.is_empty() || !grants.contains(&capability.as_str()) {
            return Err(Error::invalid("capability is not granted"));
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if head_on(&tx)? != revision {
            return Err(Error::Conflict);
        }
        let at = self.now_at(&tx)?;
        let id = insert_fact(
            &tx,
            at,
            &Fact::Acted {
                capability: capability.clone(),
                input: input.clone(),
            },
            None,
        )?;
        tx.commit()?;
        Ok(Call {
            id,
            capability,
            input,
        })
    }

    /// Read one recorded fact paired with its latest superseding receipt or concern update.
    pub fn evidence(&self, id: Id) -> Result<Evidence> {
        self.evidence_at(id, self.head()?)
    }

    pub(super) fn evidence_at(&self, id: Id, revision: Id) -> Result<Evidence> {
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
            Fact::Noticed { .. } | Fact::Said { .. } | Fact::Released { .. } => None,
        };
        Ok(Evidence {
            latest: latest.filter(|later| later.id > id),
            entry,
        })
    }

    /// An unknown receipt can be settled once. Retrying a recorded receipt returns its ID.
    pub fn finish(&self, action: Id, outcome: Outcome) -> Result<Entry> {
        let mut fact = Fact::Returned { action, outcome };
        if serde_json::to_vec(&fact)?.len() > FACT_BYTES {
            let full = match &mut fact {
                Fact::Returned {
                    outcome:
                        Outcome::Observed(text) | Outcome::Failed(text) | Outcome::Unknown(text),
                    ..
                } => std::mem::take(text),
                _ => unreachable!(),
            };
            let suffix = format!(
                "… [truncated; {} bytes; sha256:{:x}]",
                full.len(),
                Sha256::digest(full.as_bytes())
            );
            let shell_bytes = serde_json::to_vec(&fact)?.len().saturating_sub(2);
            let boundaries: Vec<usize> = full
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(full.len()))
                .collect();
            let mut lo = 0usize;
            let mut hi = boundaries.len() - 1;
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                let candidate = format!("{}{suffix}", &full[..boundaries[mid]]);
                if shell_bytes + serde_json::to_vec(&candidate)?.len() <= FACT_BYTES {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if let Fact::Returned {
                outcome: Outcome::Observed(text) | Outcome::Failed(text) | Outcome::Unknown(text),
                ..
            } = &mut fact
            {
                *text = format!("{}{suffix}", &full[..boundaries[lo]]);
            }
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
            return read_on(&tx, id);
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
        Ok(Entry { id, at, fact })
    }

    pub fn read(&self, id: Id) -> Result<Entry> {
        read_on(&self.db, id)
    }

    /// Chronological entries strictly after `after` through `through` (inclusive).
    pub fn entries_after(&self, after: Id, through: Id) -> Result<Vec<Entry>> {
        if through <= after {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .db
            .prepare("SELECT id, at, body FROM facts WHERE id > ?1 AND id <= ?2 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![after, through], read_entry_row)?;
        decode_entries(rows)
    }

    /// Actions without a receipt, in journal order.
    pub fn unfinished_actions(&self) -> Result<Vec<Id>> {
        let mut stmt = self.db.prepare(
            "SELECT action.id FROM facts AS action
             WHERE json_extract(action.body, '$.kind') = 'acted'
               AND NOT EXISTS (
                   SELECT 1 FROM facts AS receipt
                   WHERE json_extract(receipt.body, '$.kind') = 'returned'
                     AND json_extract(receipt.body, '$.action') = action.id
               )
             ORDER BY action.id",
        )?;
        let ids = stmt.query_map([], |row| row.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    /// Up to `limit` most recent entries in chronological order.
    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .db
            .prepare("SELECT id, at, body FROM facts ORDER BY id DESC LIMIT ?1")?;
        let mut entries = decode_entries(stmt.query_map([limit], read_entry_row)?)?;
        entries.reverse();
        Ok(entries)
    }

    /// All open concerns as of `revision`, most recently updated first.
    pub fn concerns(&self, revision: Id) -> Result<Vec<Concern>> {
        if revision <= 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.db.prepare(
            "SELECT f.concern, f.id, f.at, json_extract(f.body, '$.text')
             FROM facts f WHERE f.live = 1 AND f.id <= ?1
               AND NOT EXISTS (
                   SELECT 1 FROM facts later WHERE later.concern = f.concern
                       AND later.id > f.id AND later.id <= ?1
               )
             ORDER BY f.id DESC",
        )?;
        let rows = stmt.query_map([revision], |row| {
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

fn migrate_schema(db: &Connection) -> Result<()> {
    let version: i32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(Error::Storage(format!(
            "subject.db schema version {version} is newer than supported version {SCHEMA_VERSION}"
        )));
    }
    if version < 1 {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS facts (
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
             PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}
