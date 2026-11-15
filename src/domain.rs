//! Typed facts, actions, read ports and execution limits. Text is opaque.
use crate::Control;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// The one error type of a port. `Send + Sync` because a worker runs on its own
/// thread and its failure must be reportable from there.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
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
    /// A host-recorded approval of one pending proposal, for this turn. Approval
    /// is a fact like any other, never a caller's flag.
    Approval,
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

/// A pending proposal as a projection sees it. It is a view: the fact is the
/// `Proposal` event, and the section of the journal it lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalView {
    pub id: u64,
    pub kind: ProposalKind,
    pub target_id: Option<u64>,
    pub text: String,
    pub created_at: i64,
    pub turn: u64,
    /// True when the host recorded an approval for this turn.
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProposalData {
    pub(crate) kind: ProposalKind,
    #[serde(default)]
    pub(crate) target_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// A host-interpreted request for acceptance evidence. A judgment counts only
    /// when the host observed that evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseOutcome {
    pub accepted: bool,
    pub note: String,
}

/// One task, folded from its own events. A group shares this shape and never
/// executes, so a run's fields stay empty for it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The acceptance evidence this task asked the host for, fixed at open.
    pub evidence: Option<Value>,
    /// The observation that answered it, when the host produced one.
    pub evidence_id: Option<u64>,
    pub evidence_result: Option<Value>,
}

/// The model's bounded working view. A projection is derived from the journal
/// every time it is read, so it is not a fact and is not serializable.
#[derive(Debug, Clone, PartialEq)]
pub struct EjaContext {
    pub now: i64,
    pub input: String,
    pub history: Vec<Event>,
    pub goal: Option<Event>,
    pub identity: Vec<Event>,
    pub commitments: Vec<Event>,
    pub tasks: Vec<Task>,
    pub relations: Vec<Edge>,
    /// The task the projection is about; it is one of `tasks`.
    pub focus: Option<TaskId>,
    /// What this turn observed, folded from the events that recorded it. An
    /// observation names its own subject, so a repeat of the same read is the
    /// same entry and the view states the same information.
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
        /// What the host must observe before the task can be judged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Value>,
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
    /// Ask the host to bring one run's world back. The outcome, conflicts
    /// included, is recorded evidence; nothing merges silently.
    Reconcile {
        task_id: TaskId,
    },
    /// Navigation, not acquisition: what this turn is about.
    #[serde(rename = "focus")]
    Focus {
        task_id: TaskId,
    },
    #[serde(rename = "look")]
    Look {
        /// An address (`log:`, `record:`, `world:`) or a host read tool.
        #[serde(default)]
        target: String,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        selector: Selector,
    },
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

/// What one observation is. A fact says what kind it is; a target string never
/// does, or a host tool named after an authority would be read as that authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// What EJA read: an address, or a host read tool.
    #[default]
    Look,
    /// The host's answer to the acceptance evidence one task asked for.
    Witness,
    /// The host's account of one run's world.
    Reconcile,
}

/// One observation, recorded once. This is the only shape an EJA observation has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// What this is; `target` names its subject.
    #[serde(default)]
    pub kind: ObservationKind,
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

/// The host: one read primitive, and the two host acts a turn may request.
///
/// Every method inherits host control. A host that declares no worlds and no
/// acceptance evidence is still a complete host; the defaults say so.
pub trait Host {
    /// One read. A target that parses as an address resolves through that
    /// authority; any other target is a host read tool name.
    fn look(
        &self,
        target: &str,
        arguments: &Value,
        selector: &Selector,
        log: &[Event],
        control: &Control,
    ) -> Result<Value>;

    /// Observe the acceptance evidence one task asked for, at the host's discretion.
    fn witness(&self, _request: &Value, _control: &Control) -> Result<Value> {
        Err("this host observes no acceptance evidence".into())
    }

    /// Bring one run's world back into the shared world. The account, conflicts
    /// included, is evidence; the host never merges silently.
    fn reconcile(&self, _task: TaskId, _control: &Control) -> Result<Value> {
        Err("this host isolates no worlds".into())
    }
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
    /// How many worlds one decision may run at once. Each slot needs its own
    /// world, so this is also the isolation budget.
    pub slots: usize,
    pub timeout_ms: u64,
}
impl Default for TurnLimits {
    fn default() -> Self {
        Self {
            decisions: 128,
            actions: 4096,
            dispatches: 128,
            slots: 1,
            timeout_ms: 1_800_000,
        }
    }
}
impl TurnLimits {
    pub fn validate(&self) -> Result<()> {
        if self.decisions == 0
            || self.actions == 0
            || self.dispatches == 0
            || self.slots == 0
            || self.timeout_ms == 0
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
    /// Everything the turn produced, folded once: tasks, evidence and refusals
    /// are read from here, not kept in a second copy.
    pub context: EjaContext,
    pub dispatched: Vec<TaskId>,
    pub decisions: usize,
    pub worker_ms: u128,
    pub end: TurnEnd,
    pub reason: Option<String>,
}
/// Where a payload lives. One address space over every authority a view can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// A committed fact in the journal.
    Log,
    /// One entry or file of a frozen execution record.
    Record,
    /// A path in the world the host grants.
    World,
}

/// An address names one region of one authority, so a folded region and a live
/// one are read the same way: `log:event:12`, `record:3/report.md`,
/// `world:src/out.txt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub authority: Authority,
    pub id: String,
}

impl Address {
    pub fn parse(raw: &str) -> Option<Self> {
        let (scheme, id) = raw.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        let authority = match scheme {
            "log" => Authority::Log,
            "record" => Authority::Record,
            "world" => Authority::World,
            _ => return None,
        };
        Some(Self {
            authority,
            id: id.to_string(),
        })
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self.authority {
            Authority::Log => "log",
            Authority::Record => "record",
            Authority::World => "world",
        };
        write!(formatter, "{scheme}:{}", self.id)
    }
}
