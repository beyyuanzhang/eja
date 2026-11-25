use crate::{
    Call, Concern, Eja, Entry, Error, Evidence, Fact, Id, Outcome, Reflection, Result, Source,
    Timestamp, FACT_BYTES, REFLECTION_FACTS,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);
const SCHEMA_VERSION: i32 = 5;
const UNFINISHED_ACTIONS_SQL: &str = "SELECT action.id FROM facts AS action
    WHERE action.kind = 'acted'
      AND NOT EXISTS (
          SELECT 1 FROM facts AS receipt
          WHERE receipt.kind = 'returned' AND receipt.ref_id = action.id
      )
    ORDER BY action.id";

fn head_on(conn: &Connection) -> Result<Id> {
    Ok(conn.query_row("SELECT coalesce(max(id), 0) FROM facts", [], |r| r.get(0))?)
}

fn now_on(conn: &Connection, clock: &(dyn Fn() -> Timestamp + Send)) -> Result<Timestamp> {
    let last: Option<i64> = conn
        .query_row("SELECT at FROM facts ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let now = clock();
    Ok(last.map_or(now, |prev| now.max(Timestamp(prev))))
}

fn read_on(conn: &Connection, id: Id) -> Result<Entry> {
    let Some((at, body)) = conn
        .query_row("SELECT at, body FROM facts WHERE id = ?1", [id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?
    else {
        return Err(Error::invalid("unknown fact id"));
    };
    Ok(Entry {
        id,
        at: Timestamp(at),
        fact: serde_json::from_str(&body)?,
    })
}

fn query_entries(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params, |r| {
        Ok((
            r.get::<_, Id>(0)?,
            Timestamp(r.get::<_, i64>(1)?),
            r.get::<_, String>(2)?,
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

fn entries_after_on(conn: &Connection, after: Id, through: Id) -> Result<Vec<Entry>> {
    if through <= after {
        return Ok(Vec::new());
    }
    query_entries(
        conn,
        "SELECT id, at, body FROM facts WHERE id > ?1 AND id <= ?2 ORDER BY id ASC",
        params![after, through],
    )
}

fn receipt_at_on(conn: &Connection, action: Id, through: Id) -> Result<Option<Entry>> {
    conn.query_row(
        "SELECT id FROM facts WHERE kind = 'returned' AND ref_id = ?1 AND id <= ?2
         ORDER BY id DESC LIMIT 1",
        params![action, through],
        |r| r.get(0),
    )
    .optional()?
    .map(|id| read_on(conn, id))
    .transpose()
}

fn evidence_at_on(conn: &Connection, id: Id, revision: Id) -> Result<Evidence> {
    let entry = read_on(conn, id)?;
    let latest = match &entry.fact {
        Fact::Returned { action, .. } => receipt_at_on(conn, *action, revision)?,
        Fact::Acted { .. } => receipt_at_on(conn, id, revision)?,
        Fact::Kept { concern, .. } => conn
            .query_row(
                "SELECT id FROM facts WHERE live IS NOT NULL AND ref_id = ?1 AND id > ?2 AND id <= ?3
                 ORDER BY id DESC LIMIT 1",
                params![concern, id, revision],
                |r| r.get(0),
            )
            .optional()?
            .map(|later| read_on(conn, later))
            .transpose()?,
        Fact::Noticed { .. } | Fact::Said { .. } | Fact::Released { .. } | Fact::Reflected(_) => {
            None
        }
    };
    Ok(Evidence {
        latest: latest.filter(|later| later.id > id),
        entry,
    })
}

fn unfinished_actions_on(conn: &Connection) -> Result<Vec<Id>> {
    let mut stmt = conn.prepare_cached(UNFINISHED_ACTIONS_SQL)?;
    let ids = stmt.query_map([], |r| r.get(0))?;
    Ok(ids.collect::<rusqlite::Result<_>>()?)
}

fn get_meta_on(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM host_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn scope_key(when: Option<&str>) -> Option<String> {
    when.map(|text| text.trim().to_ascii_lowercase())
}

fn observations_on(conn: &Connection, through: Id) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM facts WHERE kind = 'reflected' AND id <= ?1
         AND (json_type(body, '$.next_cue') = 'text' OR json_extract(body, '$.retire') = 1)
         ORDER BY id DESC",
    )?;
    let ids = stmt
        .query_map([through], |row| row.get::<_, Id>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut seen = HashSet::new();
    let mut active = Vec::new();
    for id in ids {
        let entry = read_on(conn, id)?;
        let Fact::Reflected(reflection) = &entry.fact else {
            continue;
        };
        if seen.insert(scope_key(reflection.when.as_deref())) && !reflection.retire {
            active.push(entry);
        }
    }
    Ok(active)
}

fn observation_on(conn: &Connection, through: Id) -> Result<Option<Entry>> {
    Ok(observations_on(conn, through)?.into_iter().next())
}

fn reflection_for_on(conn: &Connection, feedback: Id) -> Result<Option<Entry>> {
    let id = conn
        .query_row(
            "SELECT id FROM facts WHERE kind = 'reflected' AND ref_id = ?1",
            [feedback],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| read_on(conn, id)).transpose()
}

fn insert_fact(
    conn: &Connection,
    at: Timestamp,
    fact: &Fact,
    ref_id: Option<Id>,
    live: Option<bool>,
) -> Result<Id> {
    let body = serde_json::to_string(fact)?;
    if body.len() > FACT_BYTES {
        return Err(Error::invalid("fact exceeds the byte budget"));
    }
    Ok(conn.query_row(
        "INSERT INTO facts(at, kind, ref_id, live, body) VALUES(?1, ?2, ?3, ?4, ?5) RETURNING id",
        params![at.as_unix(), fact.kind_name(), ref_id, live, body],
        |r| r.get(0),
    )?)
}

fn require_live_concern(conn: &Connection, concern: Id) -> Result<()> {
    let live: Option<bool> = conn
        .query_row(
            "SELECT live FROM facts WHERE live IS NOT NULL AND ref_id = ?1 ORDER BY id DESC LIMIT 1",
            [concern],
            |r| r.get(0),
        )
        .optional()?;
    if live != Some(true) {
        return Err(Error::invalid("unknown or released concern"));
    }
    Ok(())
}

fn bound_receipt_fact(action: Id, outcome: Outcome) -> Result<Fact> {
    let mut fact = Fact::Returned { action, outcome };
    if serde_json::to_vec(&fact)?.len() <= FACT_BYTES {
        return Ok(fact);
    }
    let full = match &mut fact {
        Fact::Returned {
            outcome: Outcome::Observed(t) | Outcome::Failed(t) | Outcome::Unknown(t),
            ..
        } => std::mem::take(t),
        _ => unreachable!(),
    };
    let suffix = format!(
        "… [truncated; {} bytes; sha256:{:x}]",
        full.len(),
        Sha256::digest(full.as_bytes())
    );
    let shell = serde_json::to_vec(&fact)?.len().saturating_sub(2);
    let (mut lo, mut hi) = (0usize, full.floor_char_boundary(FACT_BYTES.min(full.len())));
    while lo < hi {
        let mut cut = full.floor_char_boundary(lo + (hi - lo) / 2);
        if cut <= lo {
            cut = full.ceil_char_boundary(lo + 1);
            if cut >= hi {
                break;
            }
        }
        let candidate = format!("{}{suffix}", &full[..cut]);
        if shell + serde_json::to_vec(&candidate)?.len() <= FACT_BYTES {
            lo = cut;
        } else {
            hi = cut;
        }
    }
    if let Fact::Returned {
        outcome: Outcome::Observed(t) | Outcome::Failed(t) | Outcome::Unknown(t),
        ..
    } = &mut fact
    {
        *t = format!("{}{suffix}", &full[..lo]);
    }
    Ok(fact)
}

/// Exclusive SQLite transaction over facts and the host metadata slot.
pub struct Tx<'a> {
    tx: Transaction<'a>,
    clock: &'a (dyn Fn() -> Timestamp + Send),
}

impl Tx<'_> {
    pub fn revision(&self) -> Result<Id> {
        head_on(&self.tx)
    }

    pub fn now(&self) -> Result<Timestamp> {
        now_on(&self.tx, self.clock)
    }

    pub fn entries_after(&self, after: Id, through: Id) -> Result<Vec<Entry>> {
        entries_after_on(&self.tx, after, through)
    }

    pub fn evidence(&self, id: Id) -> Result<Evidence> {
        evidence_at_on(&self.tx, id, self.revision()?)
    }

    pub fn unfinished_actions(&self) -> Result<Vec<Id>> {
        unfinished_actions_on(&self.tx)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        get_meta_on(&self.tx, key)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        if key.is_empty() {
            return Err(Error::invalid("metadata key cannot be empty"));
        }
        self.tx.execute(
            "INSERT INTO host_meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn notice(&self, source: Source, text: impl Into<String>) -> Result<Id> {
        let at = self.now()?;
        insert_fact(
            &self.tx,
            at,
            &Fact::Noticed {
                source,
                text: text.into(),
            },
            None,
            None,
        )
    }

    pub fn say(&self, text: impl Into<String>) -> Result<Id> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(Error::invalid("reply requires non-empty text"));
        }
        let at = self.now()?;
        insert_fact(&self.tx, at, &Fact::Said { text }, None, None)
    }

    pub fn keep(&self, concern: Option<Id>, text: impl Into<String>) -> Result<Entry> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(Error::invalid("a concern needs text"));
        }
        let at = self.now()?;
        let concern = match concern {
            Some(id) => {
                require_live_concern(&self.tx, id)?;
                id
            }
            None => head_on(&self.tx)?
                .checked_add(1)
                .ok_or_else(|| Error::Storage("journal id exhausted".into()))?,
        };
        let fact = Fact::Kept { concern, text };
        let id = insert_fact(&self.tx, at, &fact, Some(concern), Some(true))?;
        Ok(Entry { id, at, fact })
    }

    pub fn release(&self, concern: Id) -> Result<Entry> {
        require_live_concern(&self.tx, concern)?;
        let at = self.now()?;
        let fact = Fact::Released { concern };
        let id = insert_fact(&self.tx, at, &fact, Some(concern), Some(false))?;
        Ok(Entry { id, at, fact })
    }

    pub fn act(&self, capability: impl Into<String>, input: Value) -> Result<Call> {
        let capability = capability.into();
        if capability.is_empty() {
            return Err(Error::invalid("action requires a non-empty capability"));
        }
        let at = self.now()?;
        let id = insert_fact(
            &self.tx,
            at,
            &Fact::Acted {
                capability: capability.clone(),
                input: input.clone(),
            },
            None,
            None,
        )?;
        Ok(Call {
            id,
            capability,
            input,
        })
    }

    pub fn finish(&self, action: Id, outcome: Outcome) -> Result<Entry> {
        let fact = bound_receipt_fact(action, outcome)?;
        if !matches!(read_on(&self.tx, action)?.fact, Fact::Acted { .. }) {
            return Err(Error::invalid("outcome needs an action id"));
        }
        let encoded = serde_json::to_string(&fact)?;
        if let Some(id) = self
            .tx
            .query_row(
                "SELECT id FROM facts WHERE kind = 'returned' AND ref_id = ?1 AND body = ?2",
                params![action, encoded],
                |r| r.get(0),
            )
            .optional()?
        {
            return read_on(&self.tx, id);
        }
        if let Some(prev) = receipt_at_on(&self.tx, action, Id::MAX)? {
            let prev_unknown = matches!(
                prev.fact,
                Fact::Returned {
                    outcome: Outcome::Unknown(_),
                    ..
                }
            );
            let next_unknown = matches!(
                fact,
                Fact::Returned {
                    outcome: Outcome::Unknown(_),
                    ..
                }
            );
            if !prev_unknown || next_unknown {
                return Err(Error::invalid("action already has a different outcome"));
            }
        }
        let at = self.now()?;
        let id = insert_fact(&self.tx, at, &fact, Some(action), None)?;
        Ok(Entry { id, at, fact })
    }
}

impl Eja {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Connection::open(path.as_ref())?;
        db.execute_batch("PRAGMA busy_timeout = 5000;")?;
        initialize_schema(&db)?;
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        Ok(Self {
            db,
            clock: Box::new(Timestamp::now),
            instance_id: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Identifies this open append-only journal within the current process.
    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }

    pub fn with_clock(mut self, clock: impl Fn() -> Timestamp + Send + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn transaction<T>(&self, f: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        let tx = Tx {
            tx: Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?,
            clock: self.clock.as_ref(),
        };
        let result = f(&tx)?;
        tx.tx.commit()?;
        Ok(result)
    }

    pub fn revision(&self) -> Result<Id> {
        head_on(&self.db)
    }

    pub fn now(&self) -> Result<Timestamp> {
        now_on(&self.db, self.clock.as_ref())
    }

    pub fn notice(&self, source: Source, text: impl Into<String>) -> Result<Id> {
        self.transaction(|tx| tx.notice(source, text))
    }

    pub fn say(&self, text: impl Into<String>) -> Result<Id> {
        self.transaction(|tx| tx.say(text))
    }

    pub fn keep(&self, concern: Option<Id>, text: impl Into<String>) -> Result<Entry> {
        self.transaction(|tx| tx.keep(concern, text))
    }

    pub fn release(&self, concern: Id) -> Result<Entry> {
        self.transaction(|tx| tx.release(concern))
    }

    /// The latest reflection carrying a changed retrieval cue at this revision.
    pub fn observation_at(&self, through: Id) -> Result<Option<Entry>> {
        observation_on(&self.db, through)
    }

    /// Active cue-bearing reflections at this revision, newest first, one per `when` scope.
    pub fn observations_at(&self, through: Id) -> Result<Vec<Entry>> {
        observations_on(&self.db, through)
    }

    pub fn reflection_for(&self, feedback: Id) -> Result<Option<Entry>> {
        reflection_for_on(&self.db, feedback)
    }

    /// Record one reflection per correction and make its new cue visible to later reflections.
    pub fn record_reflection(&self, mut reflection: Reflection) -> Result<Entry> {
        reflection.finding = reflection.finding.trim().to_owned();
        if reflection.finding.is_empty() || reflection.finding.len() > 2048 {
            return Err(Error::invalid(
                "reflection requires a finding of 1–2048 bytes",
            ));
        }
        if let Some(cue) = &mut reflection.next_cue {
            *cue = cue.trim().to_owned();
            if cue.is_empty() || cue.len() > 256 || cue.chars().any(char::is_control) {
                return Err(Error::invalid(
                    "reflection cue must be 1–256 bytes on one line",
                ));
            }
        }
        if reflection.retire && reflection.next_cue.is_some() {
            return Err(Error::invalid(
                "retiring a reflection cannot also set next_cue",
            ));
        }
        if reflection.next_cue.is_none() && !reflection.retire {
            reflection.when = None;
        } else if let Some(when) = &mut reflection.when {
            *when = when.trim().to_owned();
            if when.chars().count() < 2 || when.len() > 128 || when.chars().any(char::is_control) {
                return Err(Error::invalid(
                    "reflection scope must be at least 2 characters and at most 128 bytes on one line",
                ));
            }
        }
        self.transaction(|tx| {
            let head = tx.revision()?;
            if reflection.through > head || reflection.feedback > reflection.through {
                return Err(Error::invalid(
                    "reflection feedback is outside its snapshot",
                ));
            }
            if !matches!(
                read_on(&tx.tx, reflection.feedback)?.fact,
                Fact::Noticed {
                    source: Source::Person,
                    ..
                }
            ) {
                return Err(Error::invalid("reflection requires a person observation"));
            }
            if reflection_for_on(&tx.tx, reflection.feedback)?.is_some() {
                return Err(Error::Conflict);
            }
            let active = observations_on(&tx.tx, head)?;
            let parent_entry = match reflection.parent {
                Some(parent_id) => {
                    if parent_id > reflection.through {
                        return Err(Error::Conflict);
                    }
                    let Some(entry) = active.iter().find(|entry| entry.id == parent_id) else {
                        return Err(Error::Conflict);
                    };
                    Some(entry)
                }
                None => None,
            };
            if reflection.retire {
                let Some(Entry {
                    fact: Fact::Reflected(previous),
                    ..
                }) = parent_entry
                else {
                    return Err(Error::invalid(
                        "retiring a reflection requires an active parent",
                    ));
                };
                if reflection.when.is_some()
                    && scope_key(reflection.when.as_deref()) != scope_key(previous.when.as_deref())
                {
                    return Err(Error::invalid(
                        "retiring a reflection must match its parent scope",
                    ));
                }
                reflection.when = previous.when.clone();
            }
            let target_key = scope_key(reflection.when.as_deref());
            let existing_target = active.iter().find(|entry| match &entry.fact {
                Fact::Reflected(other) => scope_key(other.when.as_deref()) == target_key,
                _ => false,
            });
            if reflection.next_cue.is_some()
                && existing_target.is_some_and(|entry| reflection.parent != Some(entry.id))
            {
                return Err(Error::Conflict);
            }
            if reflection.shown.len() > REFLECTION_FACTS {
                return Err(Error::invalid("too many reflection observations"));
            }
            for (index, &id) in reflection.shown.iter().enumerate() {
                if id > reflection.through
                    || id == reflection.feedback
                    || reflection.shown[..index].contains(&id)
                {
                    return Err(Error::invalid("invalid reflection observation reference"));
                }
                read_on(&tx.tx, id)?;
            }
            if let Some(Entry {
                fact: Fact::Reflected(previous),
                ..
            }) = parent_entry
            {
                if scope_key(reflection.when.as_deref()) == scope_key(previous.when.as_deref())
                    && reflection.next_cue == previous.next_cue
                {
                    reflection.next_cue = None;
                    reflection.when = None;
                }
            }
            let feedback = reflection.feedback;
            let fact = Fact::Reflected(reflection);
            let at = tx.now()?;
            let id = insert_fact(&tx.tx, at, &fact, Some(feedback), None)?;
            Ok(Entry { id, at, fact })
        })
    }

    pub fn act(&self, revision: Id, capability: impl Into<String>, input: Value) -> Result<Call> {
        self.transaction(|tx| {
            if tx.revision()? != revision {
                return Err(Error::Conflict);
            }
            tx.act(capability, input)
        })
    }

    pub fn finish(&self, action: Id, outcome: Outcome) -> Result<Entry> {
        self.transaction(|tx| tx.finish(action, outcome))
    }

    pub fn read(&self, id: Id) -> Result<Entry> {
        read_on(&self.db, id)
    }

    pub fn evidence(&self, id: Id) -> Result<Evidence> {
        self.evidence_at(id, self.revision()?)
    }

    pub fn evidence_at(&self, id: Id, revision: Id) -> Result<Evidence> {
        evidence_at_on(&self.db, id, revision)
    }

    pub fn entries_after(&self, after: Id, through: Id) -> Result<Vec<Entry>> {
        entries_after_on(&self.db, after, through)
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<Entry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        query_entries(
            &self.db,
            "SELECT id, at, body FROM (SELECT id, at, body FROM facts ORDER BY id DESC LIMIT ?1)
             ORDER BY id ASC",
            [limit as i64],
        )
    }

    pub fn concerns(&self, revision: Id) -> Result<Vec<Concern>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT f.ref_id, f.id, f.at, json_extract(f.body, '$.text')
             FROM facts AS f
             WHERE f.id <= ?1 AND f.live = 1
               AND NOT EXISTS (
                   SELECT 1 FROM facts AS later
                   WHERE later.live IS NOT NULL AND later.ref_id = f.ref_id
                     AND later.id > f.id AND later.id <= ?1
               )
             ORDER BY f.id DESC",
        )?;
        let rows = stmt.query_map([revision], |r| {
            Ok(Concern {
                id: r.get(0)?,
                latest: r.get(1)?,
                at: Timestamp(r.get(2)?),
                text: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn unfinished_actions(&self) -> Result<Vec<Id>> {
        unfinished_actions_on(&self.db)
    }

    /// Return the latest unsettled `Outcome::Unknown` receipts at or before `revision`,
    /// ordered oldest-to-newest (bounded by `limit`).
    pub fn unsettled_unknowns(&self, revision: Id, limit: usize) -> Result<Vec<Entry>> {
        if limit == 0 || revision <= 0 {
            return Ok(Vec::new());
        }
        let mut entries = query_entries(
            &self.db,
            "SELECT r.id, r.at, r.body
             FROM facts AS r
             WHERE r.kind = 'returned'
               AND r.id <= ?1
               AND json_extract(r.body, '$.outcome.status') = 'unknown'
               AND NOT EXISTS (
                   SELECT 1 FROM facts AS later
                   WHERE later.kind = 'returned' AND later.ref_id = r.ref_id
                     AND later.id > r.id AND later.id <= ?1
               )
             ORDER BY r.id DESC
             LIMIT ?2",
            params![revision, limit as i64],
        )?;
        entries.reverse();
        Ok(entries)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        get_meta_on(&self.db, key)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.transaction(|tx| tx.set_meta(key, value))
    }
}

fn initialize_schema(db: &Connection) -> Result<()> {
    let schema_version =
        |conn: &Connection| conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0));
    match schema_version(db)? {
        SCHEMA_VERSION => return Ok(()),
        0 | 4 => {}
        version => return Err(unsupported_schema(version)),
    }
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    match schema_version(&tx)? {
        SCHEMA_VERSION => return Ok(()),
        4 => {
            // A reflection is a new journal fact. Old rows have the same representation.
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            return Ok(tx.commit()?);
        }
        0 => {}
        version => return Err(unsupported_schema(version)),
    }
    let populated = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema)", [], |row| {
        row.get::<_, bool>(0)
    })?;
    if populated {
        return Err(unsupported_schema(0));
    }
    tx.execute_batch(&format!(
        "CREATE TABLE facts (
             id INTEGER PRIMARY KEY, at INTEGER NOT NULL,
             kind TEXT NOT NULL, ref_id INTEGER, live INTEGER, body TEXT NOT NULL
         );
         CREATE TABLE host_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE INDEX facts_open_concerns ON facts(id) WHERE live = 1;
         CREATE INDEX facts_concern_history ON facts(ref_id, id DESC) WHERE live IS NOT NULL;
         CREATE INDEX facts_ref ON facts(kind, ref_id, id);
         PRAGMA user_version = {SCHEMA_VERSION};"
    ))?;
    tx.commit()?;
    Ok(())
}

fn unsupported_schema(version: i32) -> Error {
    Error::Storage(format!(
        "subject.db schema version {version} does not match the current version {SCHEMA_VERSION}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_rolls_back_fact_and_metadata_together() -> Result<()> {
        let eja = Eja::open(":memory:")?;
        eja.db.execute_batch(
            "CREATE TRIGGER fail_meta BEFORE INSERT ON host_meta
             BEGIN SELECT RAISE(ABORT, 'simulated failure'); END;",
        )?;
        assert!(eja
            .transaction(|tx| {
                tx.say("should roll back")?;
                tx.set_meta("cursor", "1")
            })
            .is_err());
        assert_eq!(eja.revision()?, 0);
        assert_eq!(eja.get_meta("cursor")?, None);
        eja.db.execute_batch("DROP TRIGGER fail_meta")?;
        let said = eja.transaction(|tx| {
            let id = tx.say("committed")?;
            tx.set_meta("cursor", "1")?;
            Ok(id)
        })?;
        assert_eq!(eja.revision()?, said);
        assert_eq!(eja.get_meta("cursor")?.as_deref(), Some("1"));
        Ok(())
    }

    #[test]
    fn unfinished_actions_query_searches_facts_ref_index() -> Result<()> {
        let eja = Eja::open(":memory:")?;
        let mut stmt = eja
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {UNFINISHED_ACTIONS_SQL}"))?;
        let plan: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<_>>()?;
        assert!(
            plan.iter()
                .any(|d| d.contains("SEARCH receipt USING") && d.contains("INDEX facts_ref")),
            "unexpected query plan: {plan:?}"
        );
        Ok(())
    }
}
