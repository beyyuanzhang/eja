//! Minimal continuing-subject kernel.
//! One subject per journal. Hosts supply observations, intelligence steps, and effect outcomes.
mod encoder;
pub use encoder::{Encoder, InputKind};
mod journal;
mod memory;
#[cfg(feature = "onnx")]
mod onnx;
mod time;
pub use memory::Match;
#[cfg(feature = "onnx")]
pub use onnx::OnnxEncoder;
pub use time::Timestamp;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::Path};

pub type Id = i64;
pub type Result<T> = std::result::Result<T, Error>;
/// Each view contains at most one page of recent entries and one page of open concerns.
pub const PAGE: usize = 8;
/// Each chronological replay request returns at most this many entries.
pub const REPLAY_PAGE: usize = 32;
/// Bound one serialized fact before it enters a model-facing history.
pub const FACT_BYTES: usize = 8192;

#[derive(Debug)]
pub enum Error {
    /// The journal advanced before this step was committed.
    Conflict,
    /// A step, receipt, or argument violated a kernel invariant.
    Invalid(String),
    /// Storage or serialization failure.
    Storage(String),
    /// Judge callback or encoder failure.
    External(Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => formatter.write_str("view changed before step was committed"),
            Self::Invalid(message) | Self::Storage(message) => formatter.write_str(message),
            Self::External(err) => fmt::Display::fmt(err, formatter),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::External(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Person,
    World,
    Clock,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Observed(String),
    Failed(String),
    Unknown(String),
}

/// Open a new concern (`concern: None`) or revise an existing open concern (`concern: Some(id)`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concern: Option<Id>,
    pub text: String,
}

/// Request one authorized host capability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Act {
    pub capability: String,
    pub input: Value,
}

/// One step returned by the intelligence.
/// Requests (`read`, `search`, `act`) feed new evidence back into the loop;
/// pure state updates (`keep`, `release`) or an empty step close the turn naturally.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub keep: Vec<Keep>,
    #[serde(default)]
    pub release: Vec<Id>,
    #[serde(default)]
    pub read: Vec<Id>,
    #[serde(default)]
    pub search: Vec<String>,
    #[serde(default)]
    pub act: Vec<Act>,
}

impl Step {
    pub fn has_mutations(&self) -> bool {
        !self.keep.is_empty() || !self.release.is_empty() || !self.act.is_empty()
    }

    pub fn needs_followup(&self) -> bool {
        !self.read.is_empty() || !self.search.is_empty() || !self.act.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    Noticed { source: Source, text: String },
    Kept { concern: Id, text: String },
    Released { concern: Id },
    Consolidated { through: Id, text: String },
    Acted { capability: String, input: Value },
    Returned { action: Id, outcome: Outcome },
}

/// One immutable record in the subject's journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: Id,
    pub at: Timestamp,
    pub fact: Fact,
}

/// A retrieved fact paired with its latest superseding update (receipt or concern edit), if any.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    #[serde(flatten)]
    pub entry: Entry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHits {
    pub cue: String,
    pub matches: Vec<Match>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Concern {
    pub id: Id,
    pub latest: Id,
    pub at: Timestamp,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct View {
    pub revision: Id,
    pub now: Timestamp,
    /// Most recently changed open concerns first.
    pub concerns: Vec<Concern>,
    /// Latest consolidated long-term memory, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consolidated: Option<Entry>,
    /// Newest first. Older entries are readable by id or page.
    pub recent: Vec<Entry>,
    /// Evidence returned for `step.read` from the previous step of this turn.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<Evidence>,
    /// Results returned for `step.search` from the previous step of this turn.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub searches: Vec<SearchHits>,
}

/// One bounded chronological replay window for slow memory consolidation.
#[derive(Debug, Serialize)]
pub struct Consolidation {
    pub revision: Id,
    pub now: Timestamp,
    pub concerns: Vec<Concern>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Entry>,
    /// Chronological unconsolidated entries ending at `through`.
    pub entries: Vec<Entry>,
    pub through: Id,
}

/// Delivered once, after the corresponding `Fact::Acted` is durable.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: Id,
    pub capability: String,
    pub input: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Done,
    StepLimit,
}

pub struct Eja {
    pub(crate) db: Connection,
    pub(crate) memory: Option<memory::Memory>,
    clock: Box<dyn Fn() -> Timestamp + Send>,
}

impl Eja {
    /// Open or resume one subject without semantic memory. Never initiates a call.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(Connection::open(path.as_ref())?)
    }

    /// Open or resume one subject with an explicit encoder for full-history semantic retrieval.
    pub fn open_with_encoder(
        path: impl AsRef<Path>,
        encoder: impl Encoder + 'static,
    ) -> Result<Self> {
        memory::register_vectors()?;
        let mut eja = Self::initialize(Connection::open(path.as_ref())?)?;
        eja.memory = Some(memory::Memory::attach(&eja.db, Box::new(encoder))?);
        Ok(eja)
    }

    /// Override the physical clock (defaults to system wall time).
    pub fn with_clock(mut self, clock: impl Fn() -> Timestamp + Send + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn has_encoder(&self) -> bool {
        self.memory.is_some()
    }

    pub(crate) fn clock_now(&self) -> Timestamp {
        (self.clock)()
    }

    pub fn notice(&self, source: Source, text: impl Into<String>) -> Result<Id> {
        let fact = Fact::Noticed {
            source,
            text: text.into(),
        };
        self.append_notice(&fact)
    }

    /// Pure read-only snapshot of the journal at its current revision.
    pub fn view(&self) -> Result<View> {
        let revision = self.head()?;
        Ok(View {
            revision,
            now: self.now_at(&self.db)?,
            concerns: self.concerns(revision, revision)?,
            consolidated: self.consolidated(revision)?,
            recent: self.history(revision)?,
            reads: Vec::new(),
            searches: Vec::new(),
        })
    }

    /// Slow memory consolidation loop: replays unconsolidated history in bounded chronological
    /// windows (`REPLAY_PAGE`) and commits each fold atomically as `Fact::Consolidated`.
    /// Returns the newest committed consolidation entry ID, or `None` when already caught up.
    pub fn consolidate(
        &self,
        windows: usize,
        mut distill: impl FnMut(
            &Consolidation,
        )
            -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<Option<Id>> {
        let mut latest = None;
        for _ in 0..windows {
            let Some(window) = self.unconsolidated()? else {
                break;
            };
            let text = distill(&window).map_err(Error::External)?;
            latest = Some(self.commit_consolidation(window.revision, window.through, text)?);
        }
        Ok(latest)
    }

    /// Closed-loop turn: commits mutations before external effects, feeds back query results
    /// and receipts, and terminates naturally when a step requests no new evidence or effects.
    pub fn turn(
        &self,
        grants: &[&str],
        steps: usize,
        mut judge: impl FnMut(
            &View,
        )
            -> std::result::Result<Step, Box<dyn std::error::Error + Send + Sync>>,
        mut perform: impl FnMut(Call) -> Outcome,
    ) -> Result<Stop> {
        let mut reads = Vec::new();
        let mut searches = Vec::new();
        for _ in 0..steps {
            let mut view = self.view()?;
            if view.revision == 0 {
                return Ok(Stop::Done);
            }
            view.reads = std::mem::take(&mut reads);
            view.searches = std::mem::take(&mut searches);
            let step = judge(&view).map_err(Error::External)?;
            for &fact_id in &step.read {
                if fact_id < 1 || fact_id > view.revision {
                    return Err(Error::invalid("read target must reference a recorded fact"));
                }
            }
            if !step.search.is_empty() && self.memory.is_none() {
                return Err(Error::invalid("search requires an attached encoder"));
            }
            for cue in &step.search {
                if cue.trim().is_empty() || cue.len() > FACT_BYTES {
                    return Err(Error::invalid("search requires a bounded non-empty cue"));
                }
            }
            let calls = if step.has_mutations() {
                self.commit(view.revision, &step, grants)?
            } else {
                Vec::new()
            };
            for call in calls {
                let id = call.id;
                self.finish(id, perform(call))?;
            }
            if !step.needs_followup() {
                return Ok(Stop::Done);
            }
            let head = self.head()?;
            for fact_id in step.read {
                reads.push(self.evidence(fact_id, head)?);
            }
            if let Some(memory) = &self.memory {
                let older = head.saturating_sub(PAGE as Id);
                for cue in step.search {
                    let matches = memory.search(self, &cue, older, head)?;
                    searches.push(SearchHits { cue, matches });
                }
            }
        }
        Ok(Stop::StepLimit)
    }

    /// Atomically commit state updates and authorized actions against an exact view revision.
    pub fn commit(&self, revision: Id, step: &Step, grants: &[&str]) -> Result<Vec<Call>> {
        self.commit_step(revision, step, grants)
    }
}
