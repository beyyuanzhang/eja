//! EJA core: a language-free state machine over an append-only log.
//!
//! ```text
//! L' = L + e
//! S  = fold(L)
//! C  = project(S, input, focus)
//! d  = Model(C)          // language lives in the adapter
//! S' = commit(S, d)
//! ```
//!
//! Core rules:
//! - text is opaque payload; core never tokenizes or pattern-matches it;
//! - state changes are typed actions with ids, refs and timestamps;
//! - a correction targets an event id, not a phrase;
//! - focus is an explicit task id, not a lexical match;
//! - the model adapter owns prompts, rendering and language.

pub mod adapters;
pub mod aja;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

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
    ArtifactRead,
    #[serde(rename = "decision")]
    RecordDecision,
    Relationship,
    TaskOpened,
    TaskRelation,
    TaskAssigned,
    WorkerReport,
    TaskClosed,
    Read,
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
    fn event_kind(self) -> Kind {
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
struct ProposalData {
    kind: ProposalKind,
    #[serde(default)]
    target_id: Option<u64>,
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
struct TaskRelationData {
    from: TaskId,
    to: TaskId,
    kind: RelationKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub task_id: TaskId,
    pub objective: String,
    /// What the task must satisfy; the node's exit criterion.
    #[serde(default)]
    pub acceptance: String,
    pub issued_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskOpen {
    title: String,
    request: String,
    origin: String,
    acceptance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Close {
    accepted: bool,
    note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseOutcome {
    pub accepted: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactContent {
    pub path: String,
    pub text: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl ArtifactContent {
    pub fn failed(path: &str, error: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            text: String::new(),
            truncated: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
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
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub log_path: PathBuf,
    next_id: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
}

fn read_events(path: &Path) -> Result<Vec<Event>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path)?;
    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }
    Ok(events)
}

impl Store {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        let log_path = root.as_ref().to_path_buf().join("log.jsonl");
        let existing = read_events(&log_path)?;
        let next_id = existing.iter().map(|event| event.id).max().unwrap_or(0) + 1;
        let next_turn = existing.iter().map(|event| event.turn).max().unwrap_or(0) + 1;
        Ok(Self {
            log_path,
            next_id: Arc::new(AtomicU64::new(next_id)),
            next_turn: Arc::new(AtomicU64::new(next_turn)),
        })
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
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.next_turn.fetch_max(turn + 1, Ordering::SeqCst);
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
        writeln!(file, "{}", serde_json::to_string(&event)?)?;
        file.flush()?;
        file.sync_all()?;
        Ok(event)
    }

    pub fn append_simple(&self, kind: Kind, text: &str, turn: u64, at: i64) -> Result<Event> {
        self.append(kind, "eja", text, turn, EventMeta::new(at))
    }

    pub fn next_turn(&self) -> Result<u64> {
        Ok(self.next_turn.load(Ordering::SeqCst))
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

fn closed_ids(events: &[Event]) -> HashSet<u64> {
    events
        .iter()
        .filter(|event| event.kind.is_closable() || event.kind == Kind::Correction)
        .filter_map(|event| event.closes)
        .collect()
}

pub fn recover(store: &Store, turn: u64, at: i64) -> Result<Vec<TaskId>> {
    let mut recovered = Vec::new();
    for task in tasks(&store.events()?) {
        if task.status != TaskStatus::Running {
            continue;
        }
        store.append(
            Kind::WorkerReport,
            &task_scope(task.id),
            "worker session interrupted; state unknown",
            turn,
            EventMeta::new(at),
        )?;
        recovered.push(task.id);
    }
    Ok(recovered)
}

fn goal_event(events: &[Event]) -> Option<&Event> {
    let closed = closed_ids(events);
    events
        .iter()
        .rev()
        .find(|event| event.kind == Kind::Goal && !closed.contains(&event.id))
}

pub fn current_goal(events: &[Event]) -> String {
    goal_event(events)
        .map(|event| event.text.clone())
        .unwrap_or_default()
}

pub fn open_commitments(events: &[Event]) -> Vec<Event> {
    let closed = closed_ids(events);
    events
        .iter()
        .filter(|event| event.kind == Kind::Commitment && !closed.contains(&event.id))
        .cloned()
        .collect()
}

pub fn active_identity(events: &[Event]) -> Vec<Event> {
    let mut chosen = Vec::new();
    chosen.extend(
        events
            .iter()
            .rev()
            .filter(|event| event.kind == Kind::Correction)
            .cloned(),
    );
    chosen.extend(
        events
            .iter()
            .rev()
            .filter(|event| event.kind == Kind::RecordDecision)
            .cloned(),
    );
    chosen.extend(
        events
            .iter()
            .rev()
            .filter(|event| event.kind == Kind::Relationship)
            .cloned(),
    );
    chosen.sort_by_key(|event| event.id);
    chosen.dedup_by_key(|event| event.id);
    chosen
}

fn task_scope(task_id: TaskId) -> String {
    format!("task:{task_id}")
}

fn task_id_in_scope(scope: &str) -> Option<TaskId> {
    scope.strip_prefix("task:")?.parse().ok()
}

fn next_task_id(events: &[Event]) -> TaskId {
    events
        .iter()
        .filter_map(|event| task_id_in_scope(&event.scope))
        .max()
        .unwrap_or(0)
        + 1
}

fn relations_with_tasks(events: &[Event], tasks: &[Task]) -> Vec<Edge> {
    let known: std::collections::HashSet<TaskId> = tasks.iter().map(|task| task.id).collect();
    let mut out = Vec::new();
    for event in events {
        if event.kind != Kind::TaskRelation {
            continue;
        }
        if let Some(data) = event
            .data
            .as_ref()
            .and_then(|value| serde_json::from_value::<TaskRelationData>(value.clone()).ok())
        {
            if known.contains(&data.from) && known.contains(&data.to) {
                out.push(Edge {
                    from: data.from,
                    to: data.to,
                    kind: data.kind,
                });
            }
        }
    }
    out.sort_by_key(|edge| (edge.from, edge.to, edge.kind));
    out.dedup();
    out
}

pub fn tasks(events: &[Event]) -> Vec<Task> {
    let mut out: Vec<Task> = Vec::new();
    for event in events {
        match event.kind {
            Kind::TaskOpened => {
                let Some(task_id) = task_id_in_scope(&event.scope) else {
                    continue;
                };
                let Some(open) = event
                    .data
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<TaskOpen>(value.clone()).ok())
                else {
                    continue;
                };
                out.push(Task {
                    id: task_id,
                    title: open.title,
                    request: open.request,
                    origin: open.origin,
                    acceptance: open.acceptance,
                    status: TaskStatus::Open,
                    created_at: event.at,
                    digest_at: None,
                    closed_at: None,
                    digest: None,
                    close: None,
                });
            }
            Kind::TaskAssigned => {
                let Some(task_id) = task_id_in_scope(&event.scope) else {
                    continue;
                };
                if event.data.is_some() {
                    if let Some(task) = out.iter_mut().find(|task| task.id == task_id) {
                        task.status = TaskStatus::Running;
                    }
                }
            }
            Kind::WorkerReport => {
                let Some(task_id) = task_id_in_scope(&event.scope) else {
                    continue;
                };
                if let Some(task) = out.iter_mut().find(|task| task.id == task_id) {
                    task.digest = Some(event.text.clone());
                    task.digest_at = Some(event.at);
                    task.status = TaskStatus::Waiting;
                }
            }
            Kind::TaskClosed => {
                let Some(task_id) = task_id_in_scope(&event.scope) else {
                    continue;
                };
                let Some(close) = event
                    .data
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<Close>(value.clone()).ok())
                else {
                    continue;
                };
                if let Some(task) = out.iter_mut().find(|task| task.id == task_id) {
                    task.close = Some(CloseOutcome {
                        accepted: close.accepted,
                        note: close.note,
                    });
                    task.closed_at = Some(event.at);
                    task.status = if close.accepted {
                        TaskStatus::Done
                    } else {
                        TaskStatus::Failed
                    };
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|task| task.id);
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct EjaContext {
    pub now: i64,
    pub input: String,
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

pub fn project(
    store: &Store,
    input: &str,
    focus_task: Option<TaskId>,
    now: DateTime<Utc>,
) -> Result<EjaContext> {
    project_with_evidence(store, input, focus_task, now, &[])
}

pub fn project_with_evidence(
    store: &Store,
    input: &str,
    focus_task: Option<TaskId>,
    now: DateTime<Utc>,
    evidence: &[Observation],
) -> Result<EjaContext> {
    let events = store.events()?;
    let all_tasks = tasks(&events);
    let focus = focus_task.and_then(|id| all_tasks.iter().find(|task| task.id == id).cloned());
    let mut summaries: Vec<TaskSummary> = all_tasks
        .iter()
        .map(|task| TaskSummary {
            id: task.id,
            title: task.title.clone(),
            status: task.status,
            created_at: task.created_at,
        })
        .collect();
    summaries.sort_by(|a, b| {
        b.status
            .is_active()
            .cmp(&a.status.is_active())
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(EjaContext {
        now: now.timestamp(),
        input: input.to_string(),
        goal: goal_event(&events).cloned(),
        identity: active_identity(&events),
        commitments: open_commitments(&events),
        tasks: summaries,
        relations: relations_with_tasks(&events, &all_tasks),
        focus,
        evidence: evidence.to_vec(),
        proposals: pending_proposals(&events),
        refused: turn_refusals(&events),
    })
}

fn turn_refusals(events: &[Event]) -> Vec<String> {
    let Some(turn) = events.last().map(|event| event.turn) else {
        return Vec::new();
    };
    events
        .iter()
        .filter(|event| event.kind == Kind::Refused && event.turn == turn)
        .map(|event| event.text.clone())
        .collect()
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
    #[serde(rename = "relationship", alias = "record_relationship")]
    RecordRelationship {
        text: String,
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

/// Where a look reads from: EJA's own log, a frozen node record, or the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookSource {
    Log,
    Node,
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
    Match {
        text: String,
        before: usize,
        after: usize,
    },
    Search {
        query: String,
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

/// The only read primitive. A source, a target and a selector; nothing else.
pub trait Lookup {
    fn look(
        &self,
        source: LookSource,
        target: &str,
        arguments: &Value,
        selector: &Selector,
        log: &[Event],
    ) -> Result<Value>;
}

pub struct NoopLookup;

impl Lookup for NoopLookup {
    fn look(
        &self,
        source: LookSource,
        _: &str,
        _: &Value,
        _: &Selector,
        _: &[Event],
    ) -> Result<Value> {
        Err(format!("no {source:?} reader is configured").into())
    }
}

pub trait Brain {
    fn decide(&self, context: &EjaContext) -> Decision;
}

pub trait Worker: Send + Sync {
    /// Run one node and return the host's fold of its record: what it did, how
    /// it ended and what it said last. EJA reads this and may look deeper.
    fn execute(&self, assignment: &Assignment) -> String;
    /// How many nodes this worker runs at once. A decision's dispatches are
    /// grouped by that capacity, so nothing is refused for being second.
    fn capacity(&self) -> usize {
        1
    }
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub reply: String,
    pub context: EjaContext,
    pub tasks: Vec<Task>,
    pub dispatched: Vec<TaskId>,
    /// Actions EJA asked for that the state did not allow; nothing was dropped silently.
    pub refused: Vec<String>,
    pub reads: Vec<TaskId>,
    pub evidence: Vec<Observation>,
    pub decisions: usize,
    pub worker_ms: u128,
    pub stalled: bool,
}

fn pending_proposals(events: &[Event]) -> Vec<ProposalView> {
    let resolved: HashSet<u64> = events
        .iter()
        .filter(|event| matches!(event.kind, Kind::ProposalConfirmed | Kind::ProposalRejected))
        .filter_map(|event| {
            event
                .data
                .as_ref()
                .and_then(|data| data.get("proposal_id"))
                .and_then(Value::as_u64)
        })
        .collect();
    events
        .iter()
        .filter(|event| event.kind == Kind::Proposal && !resolved.contains(&event.id))
        .filter_map(|event| {
            let data = event
                .data
                .as_ref()
                .and_then(|value| serde_json::from_value::<ProposalData>(value.clone()).ok())?;
            Some(ProposalView {
                id: event.id,
                kind: data.kind,
                target_id: data.target_id,
                text: event.text.clone(),
                created_at: event.at,
                turn: event.turn,
            })
        })
        .collect()
}

fn proposal_event(events: &[Event], proposal_id: u64) -> Option<Event> {
    events
        .iter()
        .find(|event| event.id == proposal_id && event.kind == Kind::Proposal)
        .cloned()
}

fn proposal_is_pending(events: &[Event], proposal_id: u64) -> bool {
    pending_proposals(events)
        .iter()
        .any(|proposal| proposal.id == proposal_id)
}

fn valid_proposal_target(events: &[Event], kind: ProposalKind, target_id: Option<u64>) -> bool {
    let Some(target_id) = target_id else {
        return kind != ProposalKind::Correction;
    };
    !closed_ids(events).contains(&target_id)
        && events.iter().any(|event| {
            event.id == target_id
                && event.kind.is_closable()
                && (kind == ProposalKind::Correction || event.kind == kind.event_kind())
        })
}

fn propose(
    store: &Store,
    kind: ProposalKind,
    target_id: Option<u64>,
    text: &str,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let events = store.events()?;
    if !valid_proposal_target(&events, kind, target_id) {
        return Ok(Some(format!(
            "proposal refused: no active {kind:?} target to replace"
        )));
    }
    store.append(
        Kind::Proposal,
        "eja",
        text,
        turn,
        EventMeta::data(at, serde_json::to_value(ProposalData { kind, target_id })?),
    )?;
    Ok(None)
}

fn confirm_proposal(store: &Store, proposal_id: u64, turn: u64, at: i64) -> Result<Option<String>> {
    let events = store.events()?;
    if !proposal_is_pending(&events, proposal_id) {
        return Ok(Some(format!(
            "confirm refused: proposal {proposal_id} is not pending"
        )));
    }
    let Some(event) = proposal_event(&events, proposal_id) else {
        return Ok(Some(format!(
            "confirm refused: proposal {proposal_id} is not pending"
        )));
    };
    if event.turn >= turn {
        return Ok(Some(format!(
            "confirm refused: proposal {proposal_id} was created this turn; confirm it in a later turn"
        )));
    }
    let Some(data) = event
        .data
        .as_ref()
        .and_then(|value| serde_json::from_value::<ProposalData>(value.clone()).ok())
    else {
        return Ok(Some(format!(
            "confirm refused: proposal {proposal_id} has no readable target"
        )));
    };
    if !valid_proposal_target(&events, data.kind, data.target_id) {
        return Ok(Some(format!(
            "confirm refused: the target of proposal {proposal_id} is no longer active"
        )));
    }
    // One event both establishes the replacement and closes its predecessor.
    // A Correction has no replacement: it only withdraws its target.
    store.append(
        data.kind.event_kind(),
        "eja",
        &event.text,
        turn,
        EventMeta {
            at,
            closes: data.target_id,
            data: None,
        },
    )?;
    store.append(
        Kind::ProposalConfirmed,
        "eja",
        "",
        turn,
        EventMeta::data(at, serde_json::json!({"proposal_id": proposal_id})),
    )?;
    Ok(None)
}

fn reject_proposal(
    store: &Store,
    proposal_id: u64,
    note: &str,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let events = store.events()?;
    if !proposal_is_pending(&events, proposal_id) {
        return Ok(Some(format!(
            "reject refused: proposal {proposal_id} is not pending"
        )));
    }
    store.append(
        Kind::ProposalRejected,
        "eja",
        "",
        turn,
        EventMeta::data(
            at,
            serde_json::json!({"proposal_id": proposal_id, "note": note}),
        ),
    )?;
    Ok(None)
}

fn task_status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Open => "open",
        TaskStatus::Running => "running",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
    }
}

fn append_identity(store: &Store, action: &Action, turn: u64, at: i64) -> Result<()> {
    let (kind, text) = match action {
        Action::RecordDecision { text } => (Kind::RecordDecision, text),
        Action::RecordRelationship { text } => (Kind::Relationship, text),
        _ => return Ok(()),
    };
    store.append_simple(kind, text, turn, at)?;
    Ok(())
}

struct OpenTaskInput<'a> {
    title: &'a str,
    request: &'a str,
    origin: &'a str,
    acceptance: &'a str,
    turn: u64,
    at: i64,
}

fn open_task(store: &Store, input: OpenTaskInput<'_>) -> Result<TaskId> {
    let id = next_task_id(&store.events()?);
    let data = serde_json::to_value(TaskOpen {
        title: input.title.to_string(),
        request: input.request.to_string(),
        origin: input.origin.to_string(),
        acceptance: input.acceptance.to_string(),
    })?;
    store.append(
        Kind::TaskOpened,
        &task_scope(id),
        input.title,
        input.turn,
        EventMeta::data(input.at, data),
    )?;
    Ok(id)
}

fn reserve_dispatch(
    ctx: &ActionCtx<'_>,
    task_id: TaskId,
) -> Result<std::result::Result<Assignment, String>> {
    let Some(task) = tasks(&ctx.store.events()?)
        .into_iter()
        .find(|task| task.id == task_id)
    else {
        return Ok(Err(format!("dispatch refused: no task {task_id}")));
    };
    if task.status != TaskStatus::Open {
        return Ok(Err(format!(
            "dispatch refused: task {task_id} is {}; only an Open task can be dispatched",
            task_status_name(task.status)
        )));
    }
    let assignment = Assignment {
        task_id: task.id,
        objective: task.request.clone(),
        acceptance: task.acceptance.clone(),
        issued_at: ctx.at,
    };
    ctx.store.append(
        Kind::TaskAssigned,
        &task_scope(task.id),
        "",
        ctx.turn,
        EventMeta::data(ctx.at, serde_json::to_value(&assignment)?),
    )?;
    Ok(Ok(assignment))
}

fn record_dispatch(
    ctx: &ActionCtx<'_>,
    out: &mut TurnOutcome,
    assignment: Assignment,
    digest: String,
) -> Result<()> {
    ctx.store.append(
        Kind::WorkerReport,
        &task_scope(assignment.task_id),
        &digest,
        ctx.turn,
        EventMeta::new(ctx.at),
    )?;
    out.dispatched.push(assignment.task_id);
    Ok(())
}

/// Run the reserved dispatches. One node runs inline; several run together, up
/// to the worker's declared capacity, and their reports are recorded in order.
fn run_dispatches(
    ctx: &ActionCtx<'_>,
    out: &mut TurnOutcome,
    pending: Vec<Assignment>,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let mut results: Vec<(Assignment, String)> = Vec::with_capacity(pending.len());
    let capacity = ctx.worker.capacity().max(1);
    for group in pending.chunks(capacity) {
        let started = Instant::now();
        if group.len() == 1 {
            let digest = ctx.worker.execute(&group[0]);
            results.push((group[0].clone(), digest));
        } else {
            let mut digests = Vec::with_capacity(group.len());
            let worker = ctx.worker;
            std::thread::scope(|scope| {
                let handles: Vec<_> = group
                    .iter()
                    .map(|assignment| scope.spawn(move || worker.execute(assignment)))
                    .collect();
                for handle in handles {
                    digests.push(
                        handle
                            .join()
                            .unwrap_or_else(|_| "node panicked".to_string()),
                    );
                }
            });
            results.extend(group.iter().cloned().zip(digests));
        }
        out.worker_ms += started.elapsed().as_millis();
    }
    for (assignment, digest) in results {
        record_dispatch(ctx, out, assignment, digest)?;
    }
    Ok(())
}

fn close_task(
    store: &Store,
    task_id: TaskId,
    accepted: bool,
    note: &str,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let Some(task) = tasks(&store.events()?)
        .into_iter()
        .find(|task| task.id == task_id)
    else {
        return Ok(Some(format!("close refused: no task {task_id}")));
    };
    if task.status != TaskStatus::Waiting {
        return Ok(Some(format!(
            "close refused: task {task_id} is {}; a node record is required first",
            task_status_name(task.status)
        )));
    }
    let data = serde_json::to_value(Close {
        accepted,
        note: note.to_string(),
    })?;
    store.append(
        Kind::TaskClosed,
        &task_scope(task.id),
        "",
        turn,
        EventMeta::data(at, data),
    )?;
    Ok(None)
}

struct ActionCtx<'a> {
    store: &'a Store,
    input: &'a str,
    turn: u64,
    at: i64,
    worker: &'a dyn Worker,
    lookup: &'a dyn Lookup,
}

fn apply_actions(
    actions: &[Action],
    ctx: &ActionCtx<'_>,
    out: &mut TurnOutcome,
    focus: &mut Option<TaskId>,
) -> Result<bool> {
    let mut observed = false;
    let mut pending: Vec<Assignment> = Vec::new();
    for action in actions {
        match action {
            Action::RecordDecision { .. } | Action::RecordRelationship { .. } => {
                append_identity(ctx.store, action, ctx.turn, ctx.at)?
            }
            Action::Propose {
                kind,
                target_id,
                text,
            } => {
                if let Some(note) = propose(ctx.store, *kind, *target_id, text, ctx.turn, ctx.at)? {
                    out.refused.push(note);
                }
            }
            Action::Confirm { proposal_id } => {
                if let Some(note) = confirm_proposal(ctx.store, *proposal_id, ctx.turn, ctx.at)? {
                    out.refused.push(note);
                }
            }
            Action::Reject { proposal_id, note } => {
                if let Some(note) =
                    reject_proposal(ctx.store, *proposal_id, note, ctx.turn, ctx.at)?
                {
                    out.refused.push(note);
                }
            }
            Action::OpenTask {
                title,
                request,
                acceptance,
                run,
            } => {
                let id = open_task(
                    ctx.store,
                    OpenTaskInput {
                        title,
                        request,
                        origin: ctx.input,
                        acceptance,
                        turn: ctx.turn,
                        at: ctx.at,
                    },
                )?;
                if *run {
                    match reserve_dispatch(ctx, id)? {
                        Ok(assignment) => {
                            *focus = Some(id);
                            pending.push(assignment);
                            observed = true;
                        }
                        Err(note) => out.refused.push(note),
                    }
                }
            }
            Action::Dispatch { task_id } => match reserve_dispatch(ctx, *task_id)? {
                Ok(assignment) => {
                    *focus = Some(*task_id);
                    pending.push(assignment);
                    observed = true;
                }
                Err(note) => out.refused.push(note),
            },
            Action::LinkTasks { from, to, kind } => {
                let data = serde_json::to_value(TaskRelationData {
                    from: *from,
                    to: *to,
                    kind: *kind,
                })?;
                ctx.store.append(
                    Kind::TaskRelation,
                    &task_scope(*from),
                    "",
                    ctx.turn,
                    EventMeta::data(ctx.at, data),
                )?;
            }
            Action::CloseTask {
                task_id,
                accepted,
                note,
            } => {
                if let Some(reason) =
                    close_task(ctx.store, *task_id, *accepted, note, ctx.turn, ctx.at)?
                {
                    out.refused.push(reason);
                }
            }
            Action::Focus { task_id } => {
                if !tasks(&ctx.store.events()?)
                    .iter()
                    .any(|task| task.id == *task_id)
                {
                    out.refused
                        .push(format!("focus refused: no task {task_id}"));
                    continue;
                }
                ctx.store.append(
                    Kind::Read,
                    "eja",
                    "",
                    ctx.turn,
                    EventMeta::data(ctx.at, serde_json::json!({"task_id": task_id})),
                )?;
                out.reads.push(*task_id);
                *focus = Some(*task_id);
                observed = true;
            }
            Action::Look {
                source,
                target,
                arguments,
                selector,
            } => {
                let events = ctx.store.events()?;
                let (result, error) = match ctx
                    .lookup
                    .look(*source, target, arguments, selector, &events)
                {
                    Ok(value) => (value, None),
                    Err(error) => (Value::Null, Some(error.to_string())),
                };
                let observation = Observation {
                    source: *source,
                    target: target.clone(),
                    arguments: arguments.clone(),
                    selector: selector.clone(),
                    result,
                    error,
                };
                ctx.store.append(
                    Kind::Observed,
                    "eja",
                    "",
                    ctx.turn,
                    EventMeta::data(ctx.at, serde_json::to_value(&observation)?),
                )?;
                // The view holds one entry per distinct look, not every repeat.
                out.evidence.retain(|other| {
                    !(other.source == observation.source
                        && other.target == observation.target
                        && other.arguments == observation.arguments
                        && other.selector == observation.selector)
                });
                out.evidence.push(observation);
                observed = true;
            }
        }
    }
    run_dispatches(ctx, out, pending)?;
    Ok(observed)
}

/// A turn ends at a fixpoint: a repeated projection carries no new information.
fn fingerprint(context: &EjaContext) -> u64 {
    use std::hash::{Hash, Hasher};
    let bytes = serde_json::to_vec(context).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

pub fn turn(
    store: &Store,
    input: &str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    lookup: &dyn Lookup,
) -> Result<TurnOutcome> {
    turn_at(store, input, brain, worker, lookup, now_utc())
}

pub fn turn_at(
    store: &Store,
    input: &str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    lookup: &dyn Lookup,
    now: DateTime<Utc>,
) -> Result<TurnOutcome> {
    let at = now.timestamp();
    let turn_no = store.next_turn()?;
    recover(store, turn_no, at)?;
    store.append_simple(Kind::User, input, turn_no, at)?;

    let active: Vec<Task> = tasks(&store.events()?)
        .into_iter()
        .filter(|task| task.status.is_active())
        .collect();
    let mut focus = if active.len() == 1 && active[0].status == TaskStatus::Waiting {
        Some(active[0].id)
    } else {
        None
    };
    let mut out = TurnOutcome {
        reply: String::new(),
        context: project(store, input, focus, now)?,
        tasks: Vec::new(),
        dispatched: Vec::new(),
        refused: Vec::new(),
        reads: Vec::new(),
        evidence: Vec::new(),
        decisions: 0,
        worker_ms: 0,
        stalled: false,
    };
    let ctx = ActionCtx {
        store,
        input,
        turn: turn_no,
        at,
        worker,
        lookup,
    };
    let mut seen = HashSet::new();
    seen.insert(fingerprint(&out.context));
    let mut refused_this_turn: Vec<String> = Vec::new();
    loop {
        out.decisions += 1;
        let decision = brain.decide(&out.context);
        let notes = out.refused.len();
        out.refused.extend(decision.refused.iter().cloned());
        let acted = apply_actions(&decision.actions, &ctx, &mut out, &mut focus)?;
        // A refusal is information, not a no-op: it tells EJA what the state did
        // not allow. A new note re-opens the projection so EJA can decide again;
        // a repeated note changes nothing and the fixpoint ends the turn.
        let mut told = false;
        for note in &out.refused[notes..] {
            if refused_this_turn.contains(note) {
                continue;
            }
            store.append(Kind::Refused, "eja", note, ctx.turn, EventMeta::new(at))?;
            refused_this_turn.push(note.clone());
            told = true;
        }
        let observed = acted || told;
        out.context = project_with_evidence(store, input, focus, now, &out.evidence)?;
        if !observed {
            out.reply = decision.reply.trim().to_string();
            break;
        }
        // Every look uses the same continuation path. A look that leaves the
        // projection unchanged carries no new information: the turn is over,
        // instead of stopping at an arbitrary count.
        if !seen.insert(fingerprint(&out.context)) {
            out.stalled = true;
            break;
        }
    }
    let mut meta = EventMeta::new(at);
    if out.stalled {
        meta.data = Some(serde_json::json!({"stalled": true}));
    }
    store.append(Kind::Eja, "eja", &out.reply, turn_no, meta)?;
    out.context = project_with_evidence(store, input, focus, now, &out.evidence)?;
    out.tasks = tasks(&store.events()?);
    Ok(out)
}
