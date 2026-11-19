use crate::{Concern, Eja, Entry, Fact, Id, Result, FACT_BYTES, PAGE};
use rusqlite::{params, Connection, OptionalExtension};

impl Eja {
    pub(super) fn initialize(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS facts (
                 id INTEGER PRIMARY KEY,
                 body TEXT NOT NULL,
                 concern INTEGER,
                 live INTEGER,
                 receipt_for INTEGER UNIQUE
             );
             CREATE INDEX IF NOT EXISTS concern_edits ON facts(concern, id DESC);",
        )?;
        Ok(Self(db))
    }

    pub(super) fn head(&self) -> Result<Id> {
        Ok(self
            .0
            .query_row("SELECT coalesce(max(id), 0) FROM facts", [], |r| r.get(0))?)
    }

    /// The revision check and insert share one SQLite write statement, including
    /// when independent hosts have opened the same subject.
    pub(super) fn append(
        &self,
        fact: &Fact,
        expected: Option<Id>,
        edit: Option<(Id, bool)>,
        receipt: Option<Id>,
    ) -> Result<Id> {
        let body = serde_json::to_string(fact)?;
        if body.len() > FACT_BYTES {
            return Err("fact exceeds the byte budget".into());
        }
        let (concern, live) = edit.unzip();
        let id = self
            .0
            .query_row(
                "INSERT INTO facts(body, concern, live, receipt_for)
             SELECT ?1, ?2, ?3, ?4
             WHERE ?5 IS NULL OR ?5 = (SELECT coalesce(max(id), 0) FROM facts)
             ON CONFLICT(receipt_for) DO NOTHING RETURNING id",
                params![body, concern, live, receipt, expected],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            return Ok(id);
        }
        if let Some(action) = receipt {
            // A host may retry recording its receipt. A contradictory report is refused.
            let existing = self
                .0
                .query_row(
                    "SELECT id FROM facts WHERE receipt_for = ?1 AND body = ?2",
                    params![action, body],
                    |r| r.get(0),
                )
                .optional()?;
            return existing.ok_or_else(|| "action already has a different outcome".into());
        }
        Err("view changed before judgment was committed".into())
    }

    pub fn read(&self, id: Id) -> Result<Entry> {
        let body: String = self
            .0
            .query_row("SELECT body FROM facts WHERE id = ?1", [id], |r| r.get(0))?;
        Ok(Entry {
            id,
            fact: serde_json::from_str(&body)?,
        })
    }

    /// Inclusive cursor. Continue at the last returned id minus one.
    pub fn history(&self, through: Id) -> Result<Vec<Entry>> {
        let mut stmt = self
            .0
            .prepare("SELECT id, body FROM facts WHERE id <= ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![through, PAGE], |r| {
            Ok((r.get::<_, Id>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, body) = row?;
            Ok(Entry {
                id,
                fact: serde_json::from_str(&body)?,
            })
        })
        .collect()
    }

    pub(super) fn require_open(&self, concern: Id, revision: Id) -> Result<()> {
        let live = self
            .0
            .query_row(
                "SELECT live FROM facts WHERE concern = ?1 AND id <= ?2 ORDER BY id DESC LIMIT 1",
                params![concern, revision],
                |r| r.get::<_, bool>(0),
            )
            .optional()?;
        if live == Some(true) {
            Ok(())
        } else {
            Err("unknown or released concern".into())
        }
    }

    /// Concerns as of a fixed view. Page with the last returned latest id minus one.
    pub fn concerns(&self, revision: Id, through: Id) -> Result<Vec<Concern>> {
        let mut stmt = self.0.prepare(
            "SELECT f.concern, f.id, json_extract(f.body, '$.judgment.choice.text')
             FROM facts f WHERE f.live = 1 AND f.id <= min(?1, ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM facts later WHERE later.concern = f.concern
                       AND later.id > f.id AND later.id <= ?1
               )
             ORDER BY f.id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![revision, through, PAGE], |r| {
            Ok(Concern {
                id: r.get(0)?,
                latest: r.get(1)?,
                text: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
