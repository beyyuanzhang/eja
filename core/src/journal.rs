use crate::{
    Call, Choice, Concern, Eja, Entry, Error, Evidence, Fact, Id, Judgment, Outcome, Result,
    FACT_BYTES, PAGE,
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
    let body: Option<String> = conn
        .query_row("SELECT body FROM facts WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(body) = body else {
        return Err(Error::invalid("unknown fact id"));
    };
    Ok(Entry {
        id,
        fact: serde_json::from_str(&body)?,
    })
}

fn require_open_on(conn: &Connection, concern: Id, revision: Id) -> Result<()> {
    let live = conn
        .query_row(
            "SELECT live FROM facts WHERE concern = ?1 AND id <= ?2 ORDER BY id DESC LIMIT 1",
            params![concern, revision],
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

fn require_new_action_on(conn: &Connection, proposed: &Judgment) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT body FROM facts WHERE json_extract(body,'$.judgment.choice.op')='act'
         AND json_extract(body,'$.judgment.basis')=?1",
    )?;
    let candidates = stmt.query_map([proposed.basis], |row| row.get::<_, String>(0))?;
    for body in candidates {
        if matches!(serde_json::from_str::<Fact>(&body?)?, Fact::Judged { judgment } if judgment == *proposed)
        {
            return Err(Error::invalid(
                "identical action already issued on this basis",
            ));
        }
    }
    Ok(())
}

fn insert_fact(conn: &Connection, fact: &Fact, edit: Option<(Id, bool)>) -> Result<Id> {
    let body = serde_json::to_string(fact)?;
    if body.len() > FACT_BYTES {
        return Err(Error::invalid("fact exceeds the byte budget"));
    }
    let (concern, live) = edit.unzip();
    let id = conn.query_row(
        "INSERT INTO facts(body, concern, live) VALUES(?1, ?2, ?3) RETURNING id",
        params![body, concern, live],
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
                 body TEXT NOT NULL,
                 concern INTEGER,
                 live INTEGER
             );
             CREATE INDEX IF NOT EXISTS notices ON facts(id) WHERE json_extract(body,'$.kind')='noticed';
             CREATE INDEX IF NOT EXISTS keep_events ON facts(id) WHERE live=1;
             CREATE INDEX IF NOT EXISTS concern_edits ON facts(concern, id DESC);
             CREATE INDEX IF NOT EXISTS issued_calls ON facts(json_extract(body,'$.judgment.basis'))
                 WHERE json_extract(body,'$.judgment.choice.op')='act';
             CREATE INDEX IF NOT EXISTS action_receipts ON facts(json_extract(body,'$.action'), id)
                 WHERE json_extract(body,'$.kind')='returned';",
        )?;
        Ok(Self(db, None))
    }

    pub(super) fn head(&self) -> Result<Id> {
        head_on(&self.0)
    }

    pub(super) fn append_notice(&self, fact: &Fact) -> Result<Id> {
        insert_fact(&self.0, fact, None)
    }

    pub(super) fn latest_notice_text(&self, revision: Id) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT json_extract(body,'$.text') FROM facts
                 WHERE id <= ?1 AND json_extract(body,'$.kind')='noticed'
                 ORDER BY id DESC LIMIT 1",
                [revision],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    /// All invariant checks and the insert run inside one IMMEDIATE SQLite transaction.
    pub(super) fn commit_judgment(
        &self,
        revision: Id,
        judgment: Judgment,
        grants: &[&str],
    ) -> Result<Option<Call>> {
        let tx = Transaction::new_unchecked(&self.0, TransactionBehavior::Immediate)?;
        if head_on(&tx)? != revision {
            return Err(Error::Conflict);
        }
        if judgment.basis < 1 || judgment.basis > revision {
            return Err(Error::invalid("basis must reference a recorded fact"));
        }
        let next = revision
            .checked_add(1)
            .ok_or_else(|| Error::Storage("journal id exhausted".into()))?;
        let mut call = None;
        let edit = match &judgment.choice {
            Choice::Keep { concern, text } => {
                if text.trim().is_empty() {
                    return Err(Error::invalid("a concern needs text"));
                }
                if let Some(id) = concern {
                    require_open_on(&tx, *id, revision)?;
                }
                Some((concern.unwrap_or(next), true))
            }
            Choice::Release { concern } => {
                require_open_on(&tx, *concern, revision)?;
                Some((*concern, false))
            }
            Choice::Act { capability, input } => {
                if capability.is_empty() || !grants.contains(&capability.as_str()) {
                    return Err(Error::invalid("capability is not granted"));
                }
                require_new_action_on(&tx, &judgment)?;
                call = Some(Call {
                    id: next,
                    capability: capability.clone(),
                    input: input.clone(),
                });
                None
            }
            Choice::Read { fact } => {
                if *fact < 1 || *fact > revision {
                    return Err(Error::invalid("read target must reference a recorded fact"));
                }
                None
            }
            Choice::Search { cue } => {
                if self.1.is_none() || cue.trim().is_empty() {
                    return Err(Error::invalid(
                        "search requires an attached encoder and a non-empty cue",
                    ));
                }
                None
            }
            Choice::Rest => None,
        };
        let id = insert_fact(&tx, &Fact::Judged { judgment }, edit)?;
        debug_assert_eq!(id, next);
        tx.commit()?;
        Ok(call)
    }

    pub(super) fn evidence(&self, id: Id, revision: Id) -> Result<Evidence> {
        let entry = self.read(id)?;
        let latest = match &entry.fact {
            Fact::Returned { action, .. } => receipt_at_on(&self.0, *action, revision)?,
            Fact::Judged { judgment } => match judgment.choice {
                Choice::Act { .. } => receipt_at_on(&self.0, id, revision)?,
                Choice::Keep { concern, .. } => {
                    let later = self.0.query_row(
                        "SELECT id FROM facts WHERE concern=?1 AND id>?2 AND id<=?3 ORDER BY id DESC LIMIT 1",
                        params![concern.unwrap_or(id), id, revision], |r| r.get(0),
                    ).optional()?;
                    later.map(|later_id| self.read(later_id)).transpose()?
                }
                _ => None,
            },
            Fact::Noticed { .. } => None,
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
        let tx = Transaction::new_unchecked(&self.0, TransactionBehavior::Immediate)?;
        if !matches!(
            read_on(&tx, action)?.fact,
            Fact::Judged { judgment } if matches!(judgment.choice, Choice::Act { .. })
        ) {
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
        let id = insert_fact(&tx, &fact, None)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn read(&self, id: Id) -> Result<Entry> {
        read_on(&self.0, id)
    }

    /// Inclusive cursor. Continue at the last returned id minus one.
    pub fn history(&self, through: Id) -> Result<Vec<Entry>> {
        let mut stmt = self
            .0
            .prepare("SELECT id, body FROM facts WHERE id <= ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![through, PAGE], |row| {
            Ok((row.get::<_, Id>(0)?, row.get::<_, String>(1)?))
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

    /// Concerns as of a fixed view. Page with the last returned latest id minus one.
    pub fn concerns(&self, revision: Id, through: Id) -> Result<Vec<Concern>> {
        let mut stmt = self.0.prepare(
            "SELECT f.concern, f.id, json_extract(f.body, '$.judgment.choice.text'),
                    json_extract(f.body, '$.judgment.basis')
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
                text: row.get(2)?,
                basis: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
