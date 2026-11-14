//! Typed facts, actions, read ports and execution limits. Text is opaque.
use crate::Control;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub type TaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    User,
    Eja,
    Goal,
    Commitment,
    Correction,
    Proposal,
    ProposalConfirmed,
    ProposalRejected,
    #[serde(rename = "decision")]
    RecordDecision,
    Relationship,
    TaskOpened,
    TaskRelation,
    TaskAssigned,
    WorkerReport,
    TaskClosed,
    Observed,
    /// The host rejected an action: the state did not allow it, or it could not
    /// be read as one of EJA's own actions. EJA reads these back in its projection.
    Refused,
}

impl Kind {
    pub fn is_closable(self) -> bool {
        matches!(self, Kind::Goal | Kind::Commitment)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Goal,
    Commitment,
    Correction,
}

impl ProposalKind {
    pub(crate) fn event_kind(self) -> Kind {
        match self {
            Self::Goal => Kind::Goal,
            Self::Commitment => Kind::Commitment,
            Self::Correction => Kind::Correction,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalView {
    pub id: u64,
    pub kind: ProposalKind,
    pub target_id: Option<u64>,
    pub text: String,
    pub created_at: i64,
    pub turn: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProposalData {
    pub(crate) kind: ProposalKind,
    #[serde(default)]
    pub(crate) target_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub at: i64,
    pub turn: u64,
    pub scope: String,
    pub kind: Kind,
    pub text: String,
    #[serde(default)]
    pub closes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct EventMeta {
    pub at: i64,
    pub closes: Option<u64>,
    pub data: Option<Value>,
}

impl EventMeta {
    pub fn new(at: i64) -> Self {
        Self {
            at,
            closes: None,
            data: None,
        }
    }

    pub fn data(at: i64, value: Value) -> Self {
        Self {
            at,
            closes: None,
            data: Some(value),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Aja,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    Running,
    Waiting,
    Done,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
    pub fn is_active(self) -> bool {
        matches!(
            self,
            TaskStatus::Open | TaskStatus::Running | TaskStatus::Waiting
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    PartOf,
    DependsOn,
    Continues,
    RelatesTo,
    Supersedes,
    Contradicts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: RelationKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub task_id: TaskId,
    pub objective: String,
    /// What the task must satisfy; the aja's exit criterion.
    #[serde(default)]
    pub acceptance: String,
    /// The task exactly as it arrived, so the aja works from the source and not
    /// only from EJA's reading of it.
    #[serde(default)]
    pub input: String,
    /// A predecessor whose frozen record this aja continues from.
    #[serde(default)]
    pub resume: Option<TaskId>,
    pub issued_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskOpen {
    #[serde(default)]
    pub(crate) kind: TaskKind,
    pub(crate) title: String,
    pub(crate) request: String,
    pub(crate) origin: String,
    pub(crate) acceptance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseOutcome {
    pub accepted: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub kind: TaskKind,
    pub id: TaskId,
    pub title: String,
    pub request: String,
    pub origin: String,
    pub acceptance: String,
    pub status: TaskStatus,
    pub created_at: i64,
    pub digest_at: Option<i64>,
    pub closed_at: Option<i64>,
    pub digest: Option<String>,
    pub close: Option<CloseOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub kind: TaskKind,
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EjaContext {
    pub now: i64,
    pub input: String,
    pub history: Vec<Event>,
    pub approved_proposals: Vec<u64>,
    pub goal: Option<Event>,
    pub identity: Vec<Event>,
    pub commitments: Vec<Event>,
    pub tasks: Vec<TaskSummary>,
    pub relations: Vec<Edge>,
    pub focus: Option<Task>,
    pub evidence: Vec<Observation>,
    pub proposals: Vec<ProposalView>,
    /// Actions this turn that the host could not carry out.
    pub refused: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decision {
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Actions the decision named that could not be read. The turn records them
    /// as host notes; they are never dropped silently.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub refused: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    Propose {
        kind: ProposalKind,
        #[serde(default)]
        target_id: Option<u64>,
        text: String,
    },
    Confirm {
        proposal_id: u64,
    },
    Reject {
        proposal_id: u64,
        #[serde(default)]
        note: String,
    },
    #[serde(rename = "decision")]
    RecordDecision {
        text: String,
    },
    #[serde(rename = "relationship")]
    RecordRelationship {
        text: String,
    },
    OpenGroup {
        title: String,
        acceptance: String,
    },
    OpenTask {
        title: String,
        request: String,
        acceptance: String,
        #[serde(default)]
        run: bool,
    },
    #[serde(rename = "relate")]
    LinkTasks {
        from: TaskId,
        to: TaskId,
        kind: RelationKind,
    },
    Dispatch {
        task_id: TaskId,
    },
    CloseTask {
        task_id: TaskId,
        accepted: bool,
        #[serde(default)]
        note: String,
    },
    /// Navigation, not acquisition: what this turn is about.
    #[serde(rename = "focus")]
    Focus {
        task_id: TaskId,
    },
    #[serde(rename = "look")]
    Look {
        source: LookSource,
        #[serde(default)]
        target: String,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        selector: Selector,
    },
}

/// Where a look reads from: EJA's own log, a frozen aja record, or the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookSource {
    Log,
    Aja,
    World,
}

/// How much of the target to read. One selector replaces offset/paging/query options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "select", rename_all = "snake_case")]
pub enum Selector {
    #[default]
    Whole,
    Range {
        from: usize,
        lines: usize,
    },
    Bytes {
        from: usize,
        bytes: usize,
    },
    Match {
        text: String,
        before: usize,
        after: usize,
    },
}

/// One look, recorded once. This is the only shape an EJA observation has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub source: LookSource,
    pub target: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub selector: Selector,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
}

/// The only read primitive; every invocation inherits host control.
pub trait Lookup {
    fn look(
        &self,
        source: LookSource,
        target: &str,
        arguments: &Value,
        selector: &Selector,
        log: &[Event],
        control: &Control,
    ) -> Result<Value>;
}

pub trait Brain {
    fn decide(&self, context: &EjaContext, control: &Control) -> Decision;
}

pub trait Worker: Send + Sync {
    /// Settle one execution and return the host's account, never an acceptance.
    fn execute(&self, assignment: &Assignment, control: &Control) -> String;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    Complete,
    Stalled,
    Budget,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct TurnLimits {
    pub decisions: usize,
    pub actions: usize,
    pub dispatches: usize,
    pub timeout_ms: u64,
}
impl Default for TurnLimits {
    fn default() -> Self {
        Self {
            decisions: 128,
            actions: 4096,
            dispatches: 128,
            timeout_ms: 1_800_000,
        }
    }
}
impl TurnLimits {
    pub fn validate(&self) -> Result<()> {
        if self.decisions == 0 || self.actions == 0 || self.dispatches == 0 || self.timeout_ms == 0
        {
            return Err("turn budgets must be positive".into());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    /// Approval is supplied by the host, never inferred from model output.
    pub approvals: Vec<u64>,
    pub focus: Option<TaskId>,
    pub limits: TurnLimits,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub reply: String,
    pub context: EjaContext,
    pub tasks: Vec<Task>,
    pub dispatched: Vec<TaskId>,
    /// Actions EJA asked for that the state did not allow; nothing was dropped silently.
    pub refused: Vec<String>,
    pub evidence: Vec<Observation>,
    pub decisions: usize,
    pub worker_ms: u128,
    pub end: TurnEnd,
    pub reason: Option<String>,
}
