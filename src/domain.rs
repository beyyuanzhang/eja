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
    /// The user declined one pending proposal. Like approval, a fact about the
    /// user's answer, never inferred from model output.
    Denial,
    /// The host noticed the user returned to the foreground. It is a fact about
    /// presence, not a message: it authorizes nothing and says no words.
    Returned,
    /// The host noticed the user left the foreground encounter. Tasks, proposals
    /// and memory stay exactly as they were.
    Departed,
    /// The host noticed EJA is alone: nobody has called it for a while. Like
    /// presence, it is a situation, not a message and not an order.
    Alone,
    /// EJA's own inner record: what it chooses to remember. Writing one is
    /// remembering; closing one is letting go. It is not speech to the user.
    Note,
    /// The host rejected an action: the state did not allow it, or it could not
    /// be read as one of EJA's own actions. EJA reads these back in its projection.
    Refused,
}

impl Kind {
    /// The name this kind has on the wire.
    ///
    /// Serde derives it, so it is written once: the test below holds this list to
    /// the serialized form, and a variant added without a name here fails it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Eja => "eja",
            Self::Goal => "goal",
            Self::Commitment => "commitment",
            Self::Correction => "correction",
            Self::Proposal => "proposal",
            Self::ProposalConfirmed => "proposal_confirmed",
            Self::ProposalRejected => "proposal_rejected",
            Self::RecordDecision => "decision",
            Self::Relationship => "relationship",
            Self::TaskOpened => "task_opened",
            Self::TaskRelation => "task_relation",
            Self::TaskAssigned => "task_assigned",
            Self::WorkerReport => "worker_report",
            Self::TaskClosed => "task_closed",
            Self::Observed => "observed",
            Self::Approval => "approval",
            Self::Denial => "denial",
            Self::Returned => "returned",
            Self::Departed => "departed",
            Self::Alone => "alone",
            Self::Note => "note",
            Self::Refused => "refused",
        }
    }

    #[must_use]
    pub const fn is_closable(self) -> bool {
        matches!(self, Self::Goal | Self::Commitment | Self::Note)
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
    pub(crate) const fn event_kind(self) -> Kind {
        match self {
            Self::Goal => Kind::Goal,
            Self::Commitment => Kind::Commitment,
            Self::Correction => Kind::Correction,
        }
    }
}

/// A pending proposal as a projection sees it. It is a view: the fact is the
/// `Proposal` event, and the text is borrowed from it rather than copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalView<'a> {
    pub id: u64,
    pub kind: ProposalKind,
    pub target_id: Option<u64>,
    pub text: &'a str,
    pub created_at: i64,
    pub turn: u64,
    /// True when the host recorded an approval for this turn.
    pub approved: bool,
    /// True when the user declined this proposal; it stays pending so EJA can
    /// withdraw it or ask differently.
    pub denied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalData {
    pub kind: ProposalKind,
    #[serde(default)]
    pub target_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[must_use]
    pub const fn new(at: i64) -> Self {
        Self {
            at,
            closes: None,
            data: None,
        }
    }

    #[must_use]
    pub const fn data(at: i64, value: Value) -> Self {
        Self {
            at,
            closes: None,
            data: Some(value),
        }
    }

    #[must_use]
    pub const fn closing(at: i64, target: u64) -> Self {
        Self {
            at,
            closes: Some(target),
            data: None,
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
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "open" => Some(Self::Open),
            "running" => Some(Self::Running),
            "waiting" => Some(Self::Waiting),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Open | Self::Running | Self::Waiting)
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
    /// The workspace root this run works in; empty means the host's default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace: String,
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

/// One workspace available to a turn.
///
/// `id` is the stable reference EJA may name in an action; `label` is display
/// only. `root` is the exact directory the host makes available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGrant {
    pub id: String,
    pub label: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOpen<'a> {
    #[serde(default)]
    pub(crate) kind: TaskKind,
    pub(crate) title: &'a str,
    pub(crate) request: &'a str,
    pub(crate) origin: &'a str,
    pub(crate) acceptance: &'a str,
    /// The available workspace id this task names; empty means the default.
    #[serde(default, skip_serializing_if = "str::is_empty")]
    pub(crate) workspace: &'a str,
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
    /// The available workspace root, fixed when the task was opened.
    pub workspace: String,
    pub status: TaskStatus,
    pub created_at: i64,
    pub assigned_at: Option<i64>,
    pub reported_at: Option<i64>,
    pub closed_at: Option<i64>,
    pub close: Option<CloseOutcome>,
    /// The facts of this task's own scope, in journal order.
    pub event_ids: Vec<u64>,
    /// The assignment exactly as dispatched, once it ran.
    pub assignment: Option<Value>,
    /// The facts of a task that executes. A group aggregates and never runs, so
    /// it has none of them and no reader has to ask whether a field means
    /// anything for the kind it is looking at.
    pub run: Option<Run>,
}

/// What a run leaves behind: its account, and the observation that answers its
/// acceptance contract. Every field is a fold of one recorded event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    /// The host's account of the run, from its report.
    pub digest: Option<String>,
    pub digest_at: Option<i64>,
    /// The acceptance observation this run asked for, fixed at open.
    pub evidence: Option<Value>,
    /// The observation that answered it, when the host produced one.
    pub evidence_id: Option<u64>,
    pub evidence_result: Option<Value>,
    pub witness_at: Option<i64>,
    /// The host's account of one explicit reconciliation, when it happened.
    pub reconcile: Option<Value>,
}

impl Run {
    /// A run that has not reported yet still carries the contract it was opened
    /// with, so the judgment can be made when the report arrives.
    #[must_use]
    pub fn opened(evidence: Option<Value>) -> Self {
        Self {
            evidence,
            ..Self::default()
        }
    }
}

impl Task {
    /// The acceptance observation this task asked the host for.
    #[must_use]
    pub fn evidence(&self) -> Option<&Value> {
        self.run.as_ref().and_then(|run| run.evidence.as_ref())
    }

    /// The observation that answered it, when the host produced one.
    #[must_use]
    pub fn evidence_id(&self) -> Option<u64> {
        self.run.as_ref().and_then(|run| run.evidence_id)
    }

    /// The host's account of the run, once it reported.
    #[must_use]
    pub fn digest(&self) -> Option<&str> {
        self.run.as_ref().and_then(|run| run.digest.as_deref())
    }
}

/// The model's bounded working view of the fold.
///
/// It borrows what it projects: a view is what `S` looks like within a budget,
/// never a second copy of `S`. Every field is a selection of borrowed facts or a
/// borrowed derived table.
#[derive(Debug, Clone)]
pub struct EjaContext<'a> {
    pub now: i64,
    pub input: &'a str,
    /// The workspaces the host grants. The first is the default.
    pub workspaces: &'a [WorkspaceGrant],
    pub history: Vec<&'a Event>,
    /// What EJA chose to remember: its open inner records, newest first.
    pub notes: Vec<&'a Event>,
    pub goal: Option<&'a Event>,
    pub identity: Vec<&'a Event>,
    pub commitments: Vec<&'a Event>,
    /// The task table and the graph are derived fields of the fold, so the view
    /// borrows them whole instead of copying them per decision.
    pub tasks: &'a [Task],
    pub relations: &'a [Edge],
    /// The task the projection is about; it is one of `tasks`.
    pub focus: Option<TaskId>,
    /// What this turn observed, folded from the events that recorded it. An
    /// observation names its own subject, so a repeat of the same read is the
    /// same entry and the view states the same information.
    pub evidence: Vec<Observation<'a>>,
    /// Committed observations this version could not decode. Counted, never
    /// dropped silently: the evidence section states how many are missing.
    pub unreadable_evidence: usize,
    pub proposals: Vec<ProposalView<'a>>,
    /// Actions this turn that the host could not carry out.
    pub refused: Vec<&'a str>,
}

impl EjaContext<'_> {
    /// The task the projection is about, from the one table that holds it.
    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Decision {
    #[serde(default)]
    pub reply: String,
    /// Alternatives EJA offers when the user's meaning is not unique. A label
    /// is what the user sees; the value is the exact fact their choice returns.
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Actions the decision named that could not be read. The turn records them
    /// as host notes; they are never dropped silently.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub refused: Vec<String>,
}

/// One answer EJA may offer for precise context alignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    pub value: Value,
}

/// The user's answer to one recorded choice question. `question` is the Eja
/// event that carried the choices and `option` is its zero-based position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceSelection {
    pub question: u64,
    pub option: usize,
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
    /// An inner record, not speech: writing remembers, closing lets go.
    Note {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closes: Option<u64>,
    },
    OpenGroup {
        title: String,
        acceptance: String,
    },
    OpenTask {
        title: String,
        request: String,
        acceptance: String,
        /// An available workspace id; omitted means the default workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
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
    /// Ask the host to bring one run's environment back. The outcome, conflicts
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
        /// An address (`log:`, `record:`, `environment:`) or a host read tool.
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
    /// The host's account of one run's environment.
    Reconcile,
}

/// One observation, recorded once. This is the only shape an EJA observation has.
///
/// The recorded form owns its data; the read form borrows it. `read` decodes one
/// committed event's payload in place, so a projection of a hundred observations
/// copies none of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation<'a> {
    /// What this is; `target` names its subject.
    pub kind: ObservationKind,
    pub target: &'a str,
    pub arguments: &'a Value,
    pub selector: Selector,
    pub result: &'a Value,
    pub error: Option<&'a str>,
}

impl<'a> Observation<'a> {
    /// Whether two observations answer the same read: the entry one replaces.
    #[must_use]
    pub fn same_subject(&self, other: &Observation<'_>) -> bool {
        self.target == other.target
            && self.arguments == other.arguments
            && self.selector == other.selector
    }

    /// Whether two observations state the same thing, content included. The
    /// kernel compares content, never the event that recorded it.
    #[must_use]
    pub fn same_content(&self, other: &Observation<'_>) -> bool {
        self.kind == other.kind
            && self.same_subject(other)
            && self.result == other.result
            && self.error == other.error
    }

    /// Read one recorded observation. A payload this version cannot read is not
    /// evidence, so it yields `None` and the projection skips it.
    pub fn read(data: &'a Value) -> Option<Self> {
        Some(Self {
            kind: match data.get("kind") {
                Some(kind) => ObservationKind::deserialize(kind).ok()?,
                None => ObservationKind::Look,
            },
            target: data.get("target")?.as_str()?,
            arguments: data.get("arguments")?,
            selector: match data.get("selector") {
                Some(selector) => Selector::deserialize(selector).ok()?,
                None => Selector::Whole,
            },
            result: data.get("result")?,
            error: data.get("error").and_then(Value::as_str),
        })
    }
}

/// The host: one read primitive, and the two host acts a turn may request.
///
/// Every method inherits host control. A host that declares no environments and no
/// acceptance evidence is still a complete host; the defaults say so.
pub trait Host {
    /// Resolve the workspace a task names. The host owns both the id table and
    /// the directories those ids refer to; the core only stores the answer.
    ///
    /// An omitted id selects the first grant, or no workspace when the host
    /// supplied none. An explicit id must name a grant.
    fn resolve_workspace(&self, id: Option<&str>, available: &[WorkspaceGrant]) -> Result<String> {
        let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
            return Ok(available
                .first()
                .map_or_else(String::new, |grant| grant.root.clone()));
        };
        available
            .iter()
            .find(|grant| grant.id == id)
            .map(|grant| grant.root.clone())
            .ok_or_else(|| format!("open_task refused: no available workspace id {id}").into())
    }

    /// Whether a task's already-fixed root is still one the host grants.
    ///
    /// The root is deliberately checked as stored, never re-resolved from the
    /// task's old workspace id. An empty root is a task with no workspace.
    fn workspace_available(&self, workspace: &str, available: &[WorkspaceGrant]) -> bool {
        workspace.is_empty() || available.iter().any(|grant| grant.root == workspace)
    }

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

    /// Bring one run's environment back into its shared environment. The account,
    /// conflicts
    /// included, is evidence; the host never merges silently.
    fn reconcile(
        &self,
        _task: TaskId,
        _root: Option<&std::path::Path>,
        _control: &Control,
    ) -> Result<Value> {
        Err("this host isolates no environments".into())
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
    /// How many environments one decision may run at once. Each slot needs exclusive
    /// access to an environment, so this is also the isolation budget.
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
    /// The user's declines, supplied by the host like approvals.
    pub denials: Vec<u64>,
    /// The user's answer to a question EJA asked earlier. This is alignment,
    /// not approval: its exact value becomes this turn's user input.
    pub selection: Option<ChoiceSelection>,
    pub focus: Option<TaskId>,
    /// One host situation fact opening this turn: `Returned`, `Departed` or
    /// `Alone`. The host validates the variant; nothing else may enter here.
    pub gesture: Option<Kind>,
    /// The workspaces available to this turn, default first. A task may only
    /// name one of these IDs; the fold stores the resolved root.
    pub workspaces: Vec<WorkspaceGrant>,
    pub limits: TurnLimits,
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

// `TurnOutcome` lives in `turn`: it owns the fold that produced it.

/// Where a payload lives. One address space over every authority a view can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// A committed fact in the journal.
    Log,
    /// One entry or file of a frozen execution record.
    Record,
    /// A path in an execution environment the host grants. The serialized
    /// `world` spelling remains accepted for events written before this name.
    #[serde(alias = "world")]
    Environment,
}

/// An address names one region of one authority, so a folded region and a live
/// one are read the same way.
///
/// The examples are `log:event:12`, `record:3/report.md`, and
/// `environment:src/out.txt`. The former `world:` spelling remains an input
/// alias for records and callers written before the terminology changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub authority: Authority,
    pub id: String,
}

impl Address {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let (scheme, id) = raw.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        let authority = match scheme {
            "log" => Authority::Log,
            "record" => Authority::Record,
            "environment" | "world" => Authority::Environment,
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
            Authority::Environment => "environment",
        };
        write!(formatter, "{scheme}:{}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_states_its_wire_name() {
        // serde owns the wire form; as_str is the same table written once for the
        // renderer. Reading one through the other is what keeps them one name.
        for kind in [
            Kind::User,
            Kind::Eja,
            Kind::Goal,
            Kind::Commitment,
            Kind::Correction,
            Kind::Proposal,
            Kind::ProposalConfirmed,
            Kind::ProposalRejected,
            Kind::RecordDecision,
            Kind::Relationship,
            Kind::TaskOpened,
            Kind::TaskRelation,
            Kind::TaskAssigned,
            Kind::WorkerReport,
            Kind::TaskClosed,
            Kind::Observed,
            Kind::Approval,
            Kind::Denial,
            Kind::Returned,
            Kind::Departed,
            Kind::Alone,
            Kind::Note,
            Kind::Refused,
        ] {
            let wire = serde_json::to_value(kind).expect("a unit variant serializes");
            assert_eq!(wire.as_str(), Some(kind.as_str()), "{kind:?}");
        }
    }
}
