//! Minimal continuing-subject kernel.
//! One subject per journal. Hosts supply observations, model judgments, and effect outcomes.
mod encoder;
pub use encoder::{Encoder, InputKind};
mod journal;
mod memory;
#[cfg(feature = "onnx")]
mod onnx;
pub use memory::Match;
#[cfg(feature = "onnx")]
pub use onnx::OnnxEncoder;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::Path};

pub type Id = i64;
pub type Result<T> = std::result::Result<T, Error>;
/// Each view contains at most one page of recent entries and one page of open concerns.
pub const PAGE: usize = 8;
/// Bound one serialized fact before it enters a model-facing history.
pub const FACT_BYTES: usize = 8192;

#[derive(Debug)]
pub enum Error {
    /// The journal advanced before this judgment was committed.
    Conflict,
    /// A judgment, receipt, or argument violated a kernel invariant.
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
            Self::Conflict => formatter.write_str("view changed before judgment was committed"),
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

/// The model proposes exactly one choice per view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Choice {
    /// Carry a concern forward. Supply its original id to revise it.
    Keep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        concern: Option<Id>,
        text: String,
    },
    Release {
        concern: Id,
    },
    Act {
        capability: String,
        input: Value,
    },
    /// Inspect one recorded fact by ID, paired with its latest update if any.
    Read {
        fact: Id,
    },
    /// Search older history by semantic meaning when an encoder is attached.
    Search {
        cue: String,
    },
    Rest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgment {
    /// An existing fact that grounds this judgment.
    pub basis: Id,
    pub choice: Choice,
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
    Noticed { source: Source, text: String },
    Judged { judgment: Judgment },
    Returned { action: Id, outcome: Outcome },
}

/// One immutable record in the subject's journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: Id,
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
    /// The evidence named by the latest keep judgment.
    pub basis: Id,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct View {
    pub revision: Id,
    /// Most recently changed open concerns first.
    pub concerns: Vec<Concern>,
    /// Newest first. Older entries are readable by id or page.
    pub recent: Vec<Entry>,
    /// Evidence returned by an explicit `Choice::Read` on the latest step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<Evidence>,
    /// Semantic matches from history outside `recent`, when an encoder is attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related: Option<Vec<Match>>,
}

/// Delivered once, after the corresponding judgment is durable.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: Id,
    pub capability: String,
    pub input: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Rest,
    StepLimit,
}

pub struct Eja(Connection, Option<memory::Memory>);

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
        eja.1 = Some(memory::Memory::attach(&eja.0, Box::new(encoder))?);
        Ok(eja)
    }

    pub fn notice(&self, source: Source, text: impl Into<String>) -> Result<Id> {
        let fact = Fact::Noticed {
            source,
            text: text.into(),
        };
        self.append_notice(&fact)
    }

    pub fn view(&self) -> Result<View> {
        let revision = self.head()?;
        let recent = self.history(revision)?;
        let read = match recent.first().map(|entry| &entry.fact) {
            Some(Fact::Judged {
                judgment:
                    Judgment {
                        choice: Choice::Read { fact },
                        ..
                    },
            }) => Some(self.evidence(*fact, revision)?),
            _ => None,
        };
        let related = match (&self.1, recent.last()) {
            (Some(memory), Some(oldest)) if oldest.id > 1 => {
                let cue = match recent.first().map(|entry| &entry.fact) {
                    Some(Fact::Judged {
                        judgment:
                            Judgment {
                                choice: Choice::Search { cue },
                                ..
                            },
                    }) => Some(cue.clone()),
                    Some(Fact::Judged {
                        judgment:
                            Judgment {
                                choice: Choice::Read { .. },
                                ..
                            },
                    }) => None,
                    _ => self.latest_notice_text(revision)?,
                };
                match cue {
                    Some(text) if !text.trim().is_empty() => {
                        Some(memory.search(self, &text, oldest.id - 1, revision)?)
                    }
                    _ => Some(Vec::new()),
                }
            }
            (Some(_), _) => Some(Vec::new()),
            (None, _) => None,
        };
        Ok(View {
            revision,
            concerns: self.concerns(revision, revision)?,
            recent,
            read,
            related,
        })
    }

    /// Reconsider after every fact or receipt, until rest or the host's step limit.
    /// Model and capability callbacks run outside journal transactions.
    pub fn turn(
        &self,
        grants: &[&str],
        steps: usize,
        mut judge: impl FnMut(
            &View,
        )
            -> std::result::Result<Judgment, Box<dyn std::error::Error + Send + Sync>>,
        mut perform: impl FnMut(Call) -> Outcome,
    ) -> Result<Stop> {
        for _ in 0..steps {
            let view = self.view()?;
            if view.revision == 0 {
                return Ok(Stop::Rest);
            }
            let judgment = judge(&view).map_err(Error::External)?;
            let rests = matches!(judgment.choice, Choice::Rest);
            if let Some(call) = self.decide(view.revision, judgment, grants)? {
                let id = call.id;
                self.finish(id, perform(call))?;
            }
            if rests {
                return Ok(Stop::Rest);
            }
        }
        Ok(Stop::StepLimit)
    }

    /// Commit one judgment against an exact view revision.
    pub fn decide(
        &self,
        revision: Id,
        judgment: Judgment,
        grants: &[&str],
    ) -> Result<Option<Call>> {
        self.commit_judgment(revision, judgment, grants)
    }
}
