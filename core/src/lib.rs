//! Minimal continuing-subject kernel.
//! One subject per journal. Hosts supply observations, intelligence steps, and effect outcomes.
mod journal;
mod time;
pub use journal::Tx;
pub use time::Timestamp;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

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
        }
    }
}

impl std::error::Error for Error {}

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
    #[doc(hidden)]
    #[serde(rename = "reflected")]
    LegacyReflection(Value),
}

impl Fact {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Noticed { .. } => "noticed",
            Self::Said { .. } => "said",
            Self::Kept { .. } => "kept",
            Self::Released { .. } => "released",
            Self::Acted { .. } => "acted",
            Self::Returned { .. } => "returned",
            Self::LegacyReflection(_) => "reflected",
        }
    }
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
    pub(crate) clock: Box<dyn Fn() -> Timestamp + Send>,
    pub(crate) instance_id: u64,
}
