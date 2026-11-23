//! Minimal continuing-subject kernel.
//! One subject per journal. Hosts supply observations, intelligence steps, and effect outcomes.
mod encoder;
pub use encoder::{Encoder, InputKind};
mod journal;
mod memory;
#[cfg(feature = "onnx")]
mod onnx;
mod time;
pub use journal::WakeAlarm;
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
/// Bound one serialized fact before it enters a model-facing history.
pub const FACT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum Error {
    /// The journal advanced before this step was committed.
    Conflict,
    /// A step, receipt, or argument violated a kernel invariant.
    Invalid(String),
    /// Storage or serialization failure.
    Storage(String),
    /// Encoder failure.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    Noticed {
        source: Source,
        text: String,
    },
    Said {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_response_to: Option<Source>,
    },
    Kept {
        concern: Id,
        text: String,
    },
    Released {
        concern: Id,
    },
    Acted {
        capability: String,
        input: Value,
    },
    Returned {
        action: Id,
        outcome: Outcome,
    },
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
pub struct Concern {
    pub id: Id,
    pub latest: Id,
    pub at: Timestamp,
    pub text: String,
}

/// Delivered once, after the corresponding `Fact::Acted` is durable.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: Id,
    pub capability: String,
    pub input: Value,
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
        self.append_fact(&fact)
    }

    pub fn say(&self, text: impl Into<String>) -> Result<Id> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(Error::invalid("reply requires non-empty text"));
        }
        self.append_fact(&Fact::Said {
            text,
            in_response_to: None,
        })
    }

    pub fn revision(&self) -> Result<Id> {
        self.head()
    }

    pub fn now(&self) -> Result<Timestamp> {
        self.now_at(&self.db)
    }
}
