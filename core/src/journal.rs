use crate::{
    Call, Concern, Eja, Entry, Error, Evidence, Fact, Id, Outcome, Result, Source, Timestamp,
    FACT_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i32 = 3;
const UNFINISHED_ACTIONS_SQL: &str = "SELECT action.id FROM facts AS action
    WHERE json_extract(action.body, '$.kind') = 'acted'
      AND NOT EXISTS (
          SELECT 1 FROM facts AS receipt
          WHERE json_extract(receipt.body, '$.kind') = 'returned'
            AND json_extract(receipt.body, '$.action') = +action.id
      )
    ORDER BY action.id";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct WakeAlarm {
    pub id: String,
    pub fire_at_unix: i64,
    pub fire_at_display: String,
    pub reason: String,
}

fn wake_notice(reason: &str) -> Fact {
    Fact::Noticed {
        source: Source::Clock,
        text: format!("定时唤醒：{reason}"),
    }
}

fn head_on(conn: &Connection) -> Result<Id> {
    Ok(
        conn.query_row("SELECT coalesce(max(id), 0) FROM facts", [], |row| {
            row.get(0)
        })?,
    )
}

fn next_trigger_on(conn: &Connection) -> Result<Option<(Id, Source)>> {
    Ok(conn
        .query_row(
            "SELECT id, json_extract(body,'$.source')='clock' FROM facts
        WHERE id > (SELECT through_id FROM turn_cursor WHERE singleton=1)
          AND coalesce(json_extract(body,'$.source') IN ('person','clock'), 0) = 1
        ORDER BY id LIMIT 1",
            [],
            |row| {
                let source = if row.get::<_, bool>(1)? {
                    Source::Clock
                } else {
                    Source::Person
                };
                Ok((row.get(0)?, source))
            },
        )
        .optional()?)
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

    /// Import the old host checkpoint once. An anchor-only installation can
    /// recover an interrupted action's trigger or the trailing batch of notices.
    pub fn initialize_turn_cursor(
        &self,
        legacy: Option<Id>,
        first_unfinished: Option<Id>,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM turn_cursor)", [], |row| {
            row.get::<_, bool>(0)
        })? {
            return Ok(());
        }
        let head = head_on(&tx)?;
        let through = if let Some(id) = legacy.filter(|id| (0..=head).contains(id)) {
            id
        } else if let Some(action) = first_unfinished {
            tx.query_row(
                "SELECT coalesce(max(id) - 1, ?2) FROM facts WHERE id < ?1
                AND coalesce(json_extract(body,'$.source') IN ('person','clock'), 0) = 1",
                params![action, head],
                |row| row.get(0),
            )?
        } else {
            tx.query_row("SELECT coalesce(max(id), 0) FROM facts WHERE json_extract(body,'$.kind') != 'noticed'",
                [], |row| row.get(0))?
        };
        // The previous host wrote a reply before saving its checkpoint.
        tx.execute("INSERT INTO turn_cursor(singleton, through_id)
            SELECT 1, coalesce(max(id), ?1) FROM facts WHERE id > ?1 AND json_extract(body,'$.kind')='said'", [through])?;
        tx.commit()?;
        Ok(())
    }

    pub fn next_trigger(&self) -> Result<Option<(Id, Source)>> {
        next_trigger_on(&self.db)
    }

    /// Commit the reply (including intentional silence) and consume exactly one trigger.
    pub fn complete_turn(&self, trigger: Id, reply: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let Some((id, source)) = next_trigger_on(&tx)? else {
            return Err(Error::Conflict);
        };
        if id != trigger {
            return Err(Error::Conflict);
        }
        if !reply.trim().is_empty() {
            insert_fact(
                &tx,
                self.now_at(&tx)?,
                &Fact::Said {
                    text: reply.into(),
                    in_response_to: Some(source),
                },
                None,
            )?;
        }
        tx.execute(
            "UPDATE turn_cursor SET through_id = ?1 WHERE singleton=1",
            [trigger],
        )?;
        tx.commit()?;
        Ok(())
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

    /// Project one turn's history without loading later queued requests.
    pub fn entries_for_turn(&self, after: Id, through: Id, trigger: Id) -> Result<Vec<Entry>> {
        let mut entries = self.entries_after(after, through.min(trigger))?;
        if through > trigger {
            let mut stmt = self.db.prepare_cached(
                "SELECT id, at, body FROM facts
                WHERE coalesce(json_extract(body,'$.source') IN ('person','clock'), 0) = 0
                  AND id > ?1 AND id <= ?2 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![trigger.max(after), through], read_entry_row)?;
            entries.extend(decode_entries(rows)?);
        }
        Ok(entries)
    }

    pub fn schedule_wakes(&self, alarms: &[WakeAlarm]) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        for alarm in alarms {
            if serde_json::to_vec(&wake_notice(&alarm.reason))?.len() > FACT_BYTES {
                return Err(Error::invalid("wake notice exceeds the byte budget"));
            }
            tx.execute(
                "INSERT INTO wakes(id, fire_at, display, reason) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO NOTHING",
                params![
                    alarm.id,
                    alarm.fire_at_unix,
                    alarm.fire_at_display,
                    alarm.reason
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn pending_wakes(&self) -> Result<Vec<WakeAlarm>> {
        let mut stmt = self
            .db
            .prepare("SELECT id, fire_at, display, reason FROM wakes ORDER BY fire_at, id")?;
        let rows = stmt.query_map([], |row| {
            Ok(WakeAlarm {
                id: row.get(0)?,
                fire_at_unix: row.get(1)?,
                fire_at_display: row.get(2)?,
                reason: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn next_wake_at(&self) -> Result<Option<i64>> {
        Ok(self
            .db
            .query_row("SELECT min(fire_at) FROM wakes", [], |row| row.get(0))?)
    }

    pub fn record_due_wakes(&self, now: i64) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let mut stmt =
            tx.prepare("SELECT reason FROM wakes WHERE fire_at <= ?1 ORDER BY fire_at, id")?;
        let reasons = stmt
            .query_map([now], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        if !reasons.is_empty() {
            let at = self.now_at(&tx)?;
            for reason in &reasons {
                insert_fact(&tx, at, &wake_notice(reason), None)?;
            }
            tx.execute("DELETE FROM wakes WHERE fire_at <= ?1", [now])?;
        }
        tx.commit()?;
        Ok(!reasons.is_empty())
    }

    /// Actions without a receipt, in journal order.
    pub fn unfinished_actions(&self) -> Result<Vec<Id>> {
        let mut stmt = self.db.prepare(UNFINISHED_ACTIONS_SQL)?;
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
    if version < 2 {
        db.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS wakes (
                 id TEXT PRIMARY KEY,
                 fire_at INTEGER NOT NULL,
                 display TEXT NOT NULL,
                 reason TEXT NOT NULL
             );
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    if version < 3 {
        db.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS turn_cursor(singleton INTEGER PRIMARY KEY CHECK(singleton=1), through_id INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS turn_entries ON facts(coalesce(json_extract(body,'$.source') IN ('person','clock'), 0), id);
            PRAGMA user_version = 3;
            COMMIT;")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_rolls_back_reply_and_rejects_out_of_order_completion() -> Result<()> {
        let eja = Eja::open(":memory:")?;
        eja.initialize_turn_cursor(None, None)?;
        let person = eja.notice(Source::Person, "first")?;
        let clock = eja.notice(Source::Clock, "second")?;
        assert!(matches!(
            eja.complete_turn(clock, "early"),
            Err(Error::Conflict)
        ));
        eja.db.execute_batch("CREATE TRIGGER fail_cursor BEFORE UPDATE ON turn_cursor BEGIN SELECT RAISE(ABORT,'test failure'); END;")?;
        assert!(eja.complete_turn(person, "reply").is_err());
        assert_eq!(eja.revision()?, clock);
        assert_eq!(eja.next_trigger()?, Some((person, Source::Person)));
        eja.db.execute_batch("DROP TRIGGER fail_cursor")?;
        eja.complete_turn(person, "reply")?;
        assert!(matches!(
            eja.read(clock + 1)?.fact,
            Fact::Said {
                in_response_to: Some(Source::Person),
                ..
            }
        ));
        assert!(matches!(
            eja.complete_turn(person, "duplicate"),
            Err(Error::Conflict)
        ));
        eja.complete_turn(clock, "")?;
        assert_eq!(eja.revision()?, clock + 1);
        assert_eq!(eja.next_trigger()?, None);
        Ok(())
    }

    #[test]
    fn unfinished_actions_query_searches_action_receipts_index() -> Result<()> {
        let eja = Eja::open(":memory:")?;
        let mut stmt = eja
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {UNFINISHED_ACTIONS_SQL}"))?;
        let plan: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(3))?
            .collect::<rusqlite::Result<_>>()?;
        assert!(
            plan.iter()
                .any(|detail| detail.contains("SEARCH receipt USING INDEX action_receipts")),
            "unexpected query plan: {plan:?}"
        );
        Ok(())
    }
}
