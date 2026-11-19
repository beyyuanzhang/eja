//! One subject per journal. Hosts supply observations, model judgments and effect outcomes.
mod journal;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

pub type Id = i64;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
/// Each view contains at most one page of recent entries and one page of concerns.
pub const PAGE: usize = 8;
/// Bound one serialized fact before it enters a model-facing history.
pub const FACT_BYTES: usize = 8192;

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
    /// Keep something in mind. Supply its original id to revise it.
    Keep {
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
    Rest,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgment {
    /// An existing fact that grounds this judgment.
    pub basis: Id,
    pub choice: Choice,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Observed(String),
    Failed(String),
    Unknown(String),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    Noticed { source: Source, text: String },
    Judged { judgment: Judgment },
    Returned { action: Id, outcome: Outcome },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: Id,
    pub fact: Fact,
}

#[derive(Debug, Serialize)]
pub struct Concern {
    pub id: Id,
    pub latest: Id,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct View {
    pub revision: Id,
    /// Newest first. Older entries are readable by id or page.
    pub recent: Vec<Entry>,
    /// Most recently changed first. The stable id survives a revision.
    pub concerns: Vec<Concern>,
}

/// Delivered once, after the corresponding judgment is durable.
#[derive(Debug)]
pub struct Call {
    pub id: Id,
    pub capability: String,
    pub input: Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    Rest,
    StepLimit,
}

pub struct Eja(Connection);

impl Eja {
    /// Open or resume one subject. Opening a journal never initiates a call.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(Connection::open(path)?)
    }

    pub fn notice(&self, source: Source, text: impl Into<String>) -> Result<Id> {
        let fact = Fact::Noticed {
            source,
            text: text.into(),
        };
        self.append(&fact, None, None, None)
    }

    pub fn view(&self) -> Result<View> {
        let revision = self.head()?;
        Ok(View {
            revision,
            recent: self.history(revision)?,
            concerns: self.concerns(revision, revision)?,
        })
    }

    /// Reconsider after every fact or receipt, until rest or the host's step limit.
    /// Model and capability callbacks run outside journal transactions.
    pub fn turn(
        &self,
        grants: &[&str],
        steps: usize,
        mut judge: impl FnMut(&View) -> Result<Judgment>,
        mut perform: impl FnMut(Call) -> Outcome,
    ) -> Result<Stop> {
        for _ in 0..steps {
            let view = self.view()?;
            let judgment = judge(&view)?;
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

    /// The host supplies the current grants. The model supplies only the judgment.
    pub fn decide(
        &self,
        revision: Id,
        judgment: Judgment,
        grants: &[&str],
    ) -> Result<Option<Call>> {
        if judgment.basis < 1 || judgment.basis > revision || revision != self.head()? {
            return Err("judgment needs a current view and a recorded basis".into());
        }
        let next = revision.checked_add(1).ok_or("journal id exhausted")?;
        let mut call = None;
        let edit = match &judgment.choice {
            Choice::Keep { concern, text } => {
                if text.trim().is_empty() {
                    return Err("a concern needs text".into());
                }
                if let Some(id) = concern {
                    self.require_open(*id, revision)?;
                }
                Some((concern.unwrap_or(next), true))
            }
            Choice::Release { concern } => {
                self.require_open(*concern, revision)?;
                Some((*concern, false))
            }
            Choice::Act { capability, input } => {
                if capability.is_empty() || !grants.contains(&capability.as_str()) {
                    return Err("capability is not granted".into());
                }
                call = Some(Call {
                    id: next,
                    capability: capability.clone(),
                    input: input.clone(),
                });
                None
            }
            Choice::Rest => None,
        };
        self.append(&Fact::Judged { judgment }, Some(revision), edit, None)?;
        Ok(call)
    }

    /// A host receipt establishes what the host observed about a particular call.
    pub fn finish(&self, action: Id, outcome: Outcome) -> Result<Id> {
        match self.read(action)?.fact {
            Fact::Judged {
                judgment:
                    Judgment {
                        choice: Choice::Act { .. },
                        ..
                    },
            } => {}
            _ => return Err("outcome needs an action id".into()),
        }
        let fact = Fact::Returned { action, outcome };
        self.append(&fact, None, None, Some(action))
    }
}
