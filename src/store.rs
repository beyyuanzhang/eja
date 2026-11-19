//! Durable append-only journal and its rebuildable write cursor.
use crate::{Event, EventMeta, Kind, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{BufRead, BufReader, Write},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct Store {
    pub log_path: PathBuf,
    writer: Arc<Mutex<Cursor>>,
    /// The append handle, opened once. A file has no user-space buffer, so a
    /// durable append is `sync_all`; there is no flush that reaches the disk.
    file: Arc<File>,
}

/// Exclusive right to think one turn. Holding this value across the decision and
/// its writes is the only turn-level exclusion guard.
#[derive(Debug)]
pub struct JournalLease {
    _file: File,
}

#[derive(Debug)]
struct Cursor {
    next_id: u64,
    next_turn: u64,
    bytes: u64,
}

/// How long a write may wait for the log lock before calling it a second writer.
const LOCK_WAIT_MS: u32 = 100;
/// How long one wait for the log lock sleeps before looking again.
const LOCK_POLL: Duration = Duration::from_millis(1);
const STATE_FILE: &str = "state.json";
const STATE_VERSION: u64 = 1;

/// One scan of the committed journal prefix.
///
/// A record is committed by its terminating newline. The bytes after the last
/// complete record are an uncommitted tail: they may be a complete JSON value
/// that was never committed, or a torn write. Neither is a fact. Both the kernel
/// and the HTTP view read this scan, so they cannot disagree about which event
/// ids exist.
#[derive(Debug, Clone, Default)]
pub struct JournalScan {
    pub events: Vec<Event>,
    /// Bytes occupied by complete, newline-terminated records.
    pub committed: u64,
    /// A non-empty uncommitted tail follows the committed prefix.
    pub torn: bool,
    /// Corruption inside the committed prefix. Readers must not trust any event
    /// after this point.
    pub error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateMarker {
    format: String,
    version: u64,
}

/// Open the one state format this runtime understands. A directory with any
/// existing content and no marker is not this format, so it is never interpreted
/// as a journal or upgraded in place.
fn prepare_state(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let marker = root.join(STATE_FILE);
    if marker.exists() {
        let value: StateMarker = serde_json::from_slice(&fs::read(&marker)?)?;
        if value.format != "eja" || value.version != STATE_VERSION {
            return Err(format!(
                "{} uses an unsupported EJA state format; create a new state directory",
                root.display()
            )
            .into());
        }
        return Ok(());
    }
    if fs::read_dir(root)?.next().is_some() {
        return Err(format!(
            "{} is not an initialized EJA state directory; create a new state directory",
            root.display()
        )
        .into());
    }
    let temp = root.join(".state.json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    serde_json::to_writer(&mut file, &json!({"format":"eja","version":STATE_VERSION}))?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, marker)?;
    Ok(())
}

/// The log lock guards one append, not the journal: ownership for a turn is the
/// separate `turn.lock`. A concurrent thread that spawns a child can briefly
/// duplicate the descriptor to the child, and a duplicated descriptor keeps the
/// lock alive until the child execs; that is not a second writer, so a short wait
/// tells the two apart instead of failing a turn.
fn lock_log(path: &Path, file: &File) -> Result<()> {
    let mut waited = 0;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if waited < LOCK_WAIT_MS => {
                std::thread::sleep(LOCK_POLL);
                waited += 1;
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!("log {} already has an active writer", path.display()).into())
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!("log {} cannot be locked: {error}", path.display()).into())
            }
        }
    }
}

/// Read the one committed prefix.
///
/// I/O failure is returned; corruption inside the prefix stops the scan at the
/// offending record and is reported in `error`, so a diagnostic view can show
/// the same prefix and explain the same bytes without inventing a different rule.
pub fn inspect_journal(path: impl AsRef<Path>) -> Result<JournalScan> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(JournalScan::default());
    }
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut scan = JournalScan::default();
    loop {
        let mut line = Vec::new();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            scan.torn = true;
            break;
        }
        let at = scan.committed;
        scan.committed += count as u64;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event: Event = match serde_json::from_slice(&line) {
            Ok(event) => event,
            Err(error) => {
                scan.error = Some(format!(
                    "corrupt committed log at byte {at}: {error}; the reader stops before this record"
                ));
                break;
            }
        };
        if scan
            .events
            .last()
            .is_some_and(|previous| event.id <= previous.id)
        {
            scan.error = Some(format!(
                "corrupt committed log at byte {at}: event ids must increase in append order"
            ));
            break;
        }
        scan.events.push(event);
    }
    Ok(scan)
}

fn read_events(path: &Path) -> Result<Vec<Event>> {
    let scan = inspect_journal(path)?;
    if let Some(error) = scan.error {
        return Err(error.into());
    }
    Ok(scan.events)
}

impl Store {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        prepare_state(root.as_ref())?;
        let log_path = root.as_ref().to_path_buf().join("log.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&log_path)?;
        lock_log(&log_path, &file)?;
        let scan = inspect_journal(&log_path)?;
        if let Some(error) = scan.error {
            let _ = file.unlock();
            return Err(error.into());
        }
        let existing = scan.events;
        let committed = scan.committed;
        if file.metadata()?.len() != committed {
            file.set_len(committed)?;
            file.sync_all()?;
        }
        // The handle stays open for appends; the lock covers this scan only.
        file.unlock()?;
        let next_id = existing
            .iter()
            .map(|event| event.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("event id space exhausted")?;
        let next_turn = existing
            .iter()
            .map(|event| event.turn)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("turn id space exhausted")?;
        Ok(Self {
            log_path,
            writer: Arc::new(Mutex::new(Cursor {
                next_id,
                next_turn,
                bytes: committed,
            })),
            file: Arc::new(file),
        })
    }

    /// The journal lease for one turn: held by whoever is about to think, so a
    /// second thinker (or a heartbeat about to spend a turn) stands down.
    pub fn lock_turn(&self) -> Result<JournalLease> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path.with_extension("turn.lock"))?;
        lock_log(&self.log_path, &file).map_err(|_| "another turn already owns this journal")?;
        Ok(JournalLease { _file: file })
    }

    pub fn events(&self) -> Result<Vec<Event>> {
        read_events(&self.log_path)
    }

    pub fn append(
        &self,
        kind: Kind,
        scope: &str,
        text: &str,
        turn: u64,
        meta: EventMeta,
    ) -> Result<Event> {
        let event = Event {
            id: 0,
            at: meta.at,
            turn,
            scope: scope.to_string(),
            kind,
            text: text.to_string(),
            closes: meta.closes,
            data: meta.data,
        };
        let mut cursor = self
            .writer
            .lock()
            .map_err(|_| "store writer lock poisoned")?;
        let id = cursor.next_id;
        let event = Event { id, ..event };
        lock_log(&self.log_path, &self.file)?;
        let written = self.write(&mut cursor, &event);
        // The in-process cursor is released before the log lock: the next writer
        // may be another process and must not wait on this one's bookkeeping.
        drop(cursor);
        // The lock covers this append only: the next writer may be another process.
        self.file.unlock()?;
        written?;
        Ok(event)
    }

    /// One append under the log lock: the cursor checks that nobody wrote behind
    /// this Store, the record is written and synced, and a failure rolls back.
    fn write(&self, cursor: &mut Cursor, event: &Event) -> Result<()> {
        let next_id = event.id.checked_add(1).ok_or("event id space exhausted")?;
        let next_turn = event.turn.checked_add(1).ok_or("turn id space exhausted")?;
        if self.file.metadata()?.len() != cursor.bytes {
            return Err("log changed outside this Store; reopen it before appending".into());
        }
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        // `&`File implements `Write`, so one handle serves every append.
        let mut handle: &File = &self.file;
        if let Err(error) = handle.write_all(&bytes).and_then(|()| self.file.sync_all()) {
            let _ = self.file.set_len(cursor.bytes);
            let _ = self.file.sync_all();
            return Err(error.into());
        }
        cursor.bytes += bytes.len() as u64;
        cursor.next_id = next_id;
        cursor.next_turn = cursor.next_turn.max(next_turn);
        Ok(())
    }

    pub fn append_simple(&self, kind: Kind, text: &str, turn: u64, at: i64) -> Result<Event> {
        self.append(kind, "eja", text, turn, EventMeta::new(at))
    }

    pub fn next_turn(&self) -> Result<u64> {
        Ok(self
            .writer
            .lock()
            .map_err(|_| "store writer lock poisoned")?
            .next_turn)
    }
}

#[must_use]
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// A read-only look at the journal: no repair, no cursor, no writer claim.
pub fn read_journal(root: impl AsRef<Path>) -> Result<Vec<Event>> {
    read_events(&root.as_ref().join("log.jsonl"))
}

#[must_use]
pub fn format_utc(at: i64) -> String {
    DateTime::<Utc>::from_timestamp(at, 0).map_or_else(
        || at.to_string(),
        |time| time.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}
