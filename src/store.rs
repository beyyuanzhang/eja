//! Durable append-only journal and its rebuildable write cursor.
use crate::{Event, EventMeta, Kind, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub struct Store {
    pub log_path: PathBuf,
    writer: Arc<Mutex<Cursor>>,
    pub(crate) active_turn: Arc<Mutex<()>>,
}

#[derive(Debug)]
struct Cursor {
    next_id: u64,
    next_turn: u64,
    bytes: u64,
}

fn lock_log(file: &fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("log already has an active writer".into());
        }
    }
    Ok(())
}

/// A complete newline commits a record. Only an uncommitted final fragment is
/// recoverable; corruption inside the committed prefix is always an error.
fn scan_events(path: &Path) -> Result<(Vec<Event>, u64)> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut events: Vec<Event> = Vec::new();
    let mut committed = 0u64;
    loop {
        let mut line = Vec::new();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 || line.last() != Some(&b'\n') {
            break;
        }
        committed += count as u64;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event: Event = serde_json::from_slice(&line).map_err(|error| {
            format!(
                "corrupt committed log at byte {}: {error}",
                committed - count as u64
            )
        })?;
        if events
            .last()
            .is_some_and(|previous| event.id <= previous.id)
        {
            return Err("event ids must increase in append order".into());
        }
        events.push(event);
    }
    Ok((events, committed))
}

fn read_events(path: &Path) -> Result<Vec<Event>> {
    Ok(scan_events(path)?.0)
}

impl Store {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        let log_path = root.as_ref().to_path_buf().join("log.jsonl");
        let locked = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&log_path)?;
        lock_log(&locked)?;
        let (existing, committed) = scan_events(&log_path)?;
        if log_path.exists() && fs::metadata(&log_path)?.len() != committed {
            let file = OpenOptions::new().write(true).open(&log_path)?;
            file.set_len(committed)?;
            file.sync_all()?;
        }
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
            active_turn: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) fn lock_turn(&self) -> Result<fs::File> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path.with_extension("turn.lock"))?;
        lock_log(&file).map_err(|_| "another turn already owns this journal")?;
        Ok(file)
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
        let mut cursor = self
            .writer
            .lock()
            .map_err(|_| "store writer lock poisoned")?;
        let id = cursor.next_id;
        let next_id = id.checked_add(1).ok_or("event id space exhausted")?;
        let next_turn = turn.checked_add(1).ok_or("turn id space exhausted")?;
        let event = Event {
            id,
            at: meta.at,
            turn,
            scope: scope.to_string(),
            kind,
            text: text.to_string(),
            closes: meta.closes,
            data: meta.data,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        lock_log(&file)?;
        if file.metadata()?.len() != cursor.bytes {
            return Err("log changed outside this Store; reopen it before appending".into());
        }
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        if let Err(error) = file
            .write_all(&bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
        {
            let _ = file.set_len(cursor.bytes);
            let _ = file.sync_all();
            return Err(error.into());
        }
        cursor.bytes += bytes.len() as u64;
        cursor.next_id = next_id;
        cursor.next_turn = cursor.next_turn.max(next_turn);
        Ok(event)
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

pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

pub fn format_utc(at: i64) -> String {
    DateTime::<Utc>::from_timestamp(at, 0)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| at.to_string())
}
