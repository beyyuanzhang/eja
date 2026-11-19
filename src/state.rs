//! `S = fold(L)`: the one value derived from committed facts.
//!
//! A turn folds the journal once and pushes every event it commits into the same
//! value, so validation and projection read one derivation instead of scanning
//! the log again per action. Nothing here executes an effect, and a projection
//! never changes a fact.

use crate::domain::{ProposalData, TaskOpen};
use crate::{
    CloseOutcome, Edge, EjaContext, Event, Kind, Observation, ObservationKind, ProposalKind,
    ProposalView, Result, Run, Store, Task, TaskId, TaskKind, TaskStatus, WorkspaceGrant,
};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

/// What one turn observed: the readable observations, and how many committed
/// ones this version could not decode. A count, so nothing is dropped silently.
#[derive(Debug, Default, Clone)]
pub struct Evidence<'a> {
    pub observations: Vec<Observation<'a>>,
    pub unreadable: usize,
}

impl Evidence<'_> {
    pub const fn len(&self) -> usize {
        self.observations.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }
}

/// A proposal that no event has resolved yet. Its own event id is its identity.
#[derive(Debug, Clone)]
pub struct OpenProposal {
    pub id: u64,
    pub data: ProposalData,
    pub text: String,
    pub at: i64,
    pub turn: u64,
}

/// The logical state: durable facts folded once.
#[derive(Debug, Clone, Default)]
pub struct State {
    events: Vec<Event>,
    /// Every event id that some event closed.
    closed: HashSet<u64>,
    /// Indices of `Goal` events, oldest first; the newest unclosed one is current.
    goals: Vec<usize>,
    commitments: Vec<usize>,
    identity: Vec<usize>,
    /// Indices of `Note` events, oldest first; open ones are what EJA remembers.
    notes: Vec<usize>,
    /// Tasks by id.
    tasks: Vec<Task>,
    /// Execution and containment edges, sorted and unique.
    edges: Vec<Edge>,
    /// Edges that named a task the log had not opened yet.
    dangling: Vec<Edge>,
    proposals: Vec<OpenProposal>,
    /// `(turn, proposal)` pairs the host approved.
    approvals: Vec<(u64, u64)>,
    /// Proposals the user declined; the fact lives in the journal, the fold
    /// only needs the ids.
    denials: Vec<u64>,
    /// Turns with a user presence fact. This is the one derivation used by both
    /// the model view and the API's spoken history.
    present_turns: HashSet<u64>,
    /// The highest task id this log has allocated, decodable or not.
    last_task_id: TaskId,
}

/// Decode one fact's payload. `&Value` is a deserializer, so the payload is read
/// in place instead of cloned into an owned tree first.
fn decode<'de, T: serde::Deserialize<'de>>(event: &'de Event) -> Option<T> {
    event
        .data
        .as_ref()
        .and_then(|value| T::deserialize(value).ok())
}

fn proposal_of(event: &Event) -> Option<u64> {
    event
        .data
        .as_ref()
        .and_then(|data| data.get("proposal_id"))
        .and_then(serde_json::Value::as_u64)
}

pub fn task_scope(task_id: TaskId) -> String {
    format!("task:{task_id}")
}

fn task_id_in_scope(scope: &str) -> Option<TaskId> {
    scope.strip_prefix("task:")?.parse().ok()
}

impl State {
    /// Fold the committed journal.
    pub fn load(store: &Store) -> Result<Self> {
        Ok(Self::fold(store.events()?))
    }

    #[must_use]
    pub fn fold(events: Vec<Event>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.push(event);
        }
        state
    }

    /// Apply one committed fact and keep it. This is the fold's only transition:
    /// the initial fold and every append inside a turn run the same code.
    pub fn push(&mut self, event: Event) {
        self.apply(&event);
        self.events.push(event);
    }

    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The newest committed fact identifies everything committed so far.
    #[must_use]
    pub fn last_id(&self) -> u64 {
        self.events.last().map_or(0, |event| event.id)
    }

    /// The turn the newest fact belongs to.
    #[must_use]
    pub fn turn(&self) -> u64 {
        self.events.last().map_or(0, |event| event.turn)
    }

    #[must_use]
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    #[must_use]
    pub fn knows(&self, id: TaskId) -> bool {
        self.task(id).is_some()
    }

    #[must_use]
    pub fn relations(&self) -> &[Edge] {
        &self.edges
    }

    pub fn next_task_id(&self) -> Result<TaskId> {
        self.last_task_id
            .checked_add(1)
            .ok_or_else(|| "task id space exhausted".into())
    }

    /// The newest unclosed goal. Adding one without replacing leaves the others
    /// active, so withdrawing the newest reveals the next one.
    #[must_use]
    pub fn goal(&self) -> Option<&Event> {
        self.goals
            .iter()
            .rev()
            .filter_map(|index| self.events.get(*index))
            .find(|event| !self.closed.contains(&event.id))
    }

    #[must_use]
    pub fn commitments(&self) -> Vec<&Event> {
        self.open(&self.commitments)
    }

    /// What EJA chose to remember: open inner records, newest first.
    #[must_use]
    pub fn notes(&self) -> Vec<&Event> {
        let mut open = self.open(&self.notes);
        open.reverse();
        open
    }

    /// Every open proposal, oldest first, whatever turn it was created in.
    #[must_use]
    pub fn proposals(&self) -> &[OpenProposal] {
        &self.proposals
    }

    #[must_use]
    pub fn identity(&self) -> Vec<&Event> {
        self.identity
            .iter()
            .filter_map(|index| self.events.get(*index))
            .collect()
    }

    /// Whether the user was present in one turn. EJA's own automatic approval is
    /// not presence; a host approval carrying the user's answer is.
    #[must_use]
    pub fn present(&self, turn: u64) -> bool {
        self.present_turns.contains(&turn)
    }

    /// Actions this turn that the host could not carry out, from EJA's own words.
    ///
    /// A turn's facts are the journal's tail, so the scan stops at its first
    /// event and reading this turn costs this turn, not the whole log. There is
    /// no argument to get wrong: the turn is the newest one by definition.
    #[must_use]
    pub fn refusals(&self) -> Vec<&str> {
        let turn = self.turn();
        self.events
            .iter()
            .rev()
            .take_while(|event| event.turn == turn)
            .filter(|event| matches!(event.kind, Kind::Refused | Kind::Failure))
            .map(|event| event.text.as_str())
            .collect()
    }

    /// What this turn observed, one entry per distinct observation: a repeated
    /// read keeps the newest answer, so the view states what is currently true.
    /// What this turn observed, and what of it this version could not read.
    ///
    /// A committed observation this version cannot decode is counted rather than
    /// dropped: the projection states how many it could not show.
    pub fn evidence(&self) -> Evidence<'_> {
        let turn = self.turn();
        let mut evidence = Evidence::default();
        // Walk the tail backwards, then restore the order the turn happened in.
        for event in self
            .events
            .iter()
            .rev()
            .take_while(|event| event.turn == turn)
            .filter(|event| event.kind == Kind::Observed)
        {
            let read = event.data.as_ref().and_then(Observation::read);
            let Some(observation) = read else {
                evidence.unreadable += 1;
                continue;
            };
            // A repeated read is one entry: the newest answer is what holds.
            if evidence
                .observations
                .iter()
                .any(|other: &Observation<'_>| other.same_subject(&observation))
            {
                continue;
            }
            evidence.observations.push(observation);
        }
        evidence.observations.reverse();
        evidence
    }

    /// Approvals the host recorded for one turn.
    #[must_use]
    pub fn approved(&self, turn: u64) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .approvals
            .iter()
            .filter(|(approved_in, _)| *approved_in == turn)
            .map(|(_, proposal)| *proposal)
            .collect();
        // A correction asks the user nothing, so the host's approval of it is
        // not bound to one turn: it stands until the withdrawal is confirmed.
        for proposal in &self.proposals {
            if proposal.data.kind == ProposalKind::Correction
                && self
                    .approvals
                    .iter()
                    .any(|(approved_in, approved)| *approved == proposal.id && *approved_in <= turn)
            {
                out.push(proposal.id);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    #[must_use]
    pub fn pending_proposals(&self, turn: u64) -> Vec<ProposalView<'_>> {
        let approved = self.approved(turn);
        self.proposals
            .iter()
            .map(|proposal| ProposalView {
                id: proposal.id,
                kind: proposal.data.kind,
                target_id: proposal.data.target_id,
                text: &proposal.text,
                created_at: proposal.at,
                turn: proposal.turn,
                approved: approved.contains(&proposal.id),
                denied: self.denials.contains(&proposal.id),
            })
            .collect()
    }

    /// A proposal may only name an active identity fact of its own kind: a
    /// correction withdraws its target, a goal or commitment replaces it. A
    /// correction may also withdraw a still-pending proposal.
    #[must_use]
    pub fn valid_proposal_target(&self, kind: ProposalKind, target_id: Option<u64>) -> bool {
        let Some(target_id) = target_id else {
            return kind != ProposalKind::Correction;
        };
        if kind == ProposalKind::Correction
            && self
                .proposals
                .iter()
                .any(|proposal| proposal.id == target_id)
        {
            return true;
        }
        !self.closed.contains(&target_id)
            && self.events.iter().any(|event| {
                event.id == target_id
                    && event.kind.is_closable()
                    && (kind == ProposalKind::Correction || event.kind == kind.event_kind())
            })
    }

    /// The model's bounded working view: the projection of this state. It
    /// borrows every fact it shows, so a decision costs no copy of the log.
    #[must_use]
    pub fn project<'a>(
        &'a self,
        input: &'a str,
        focus: Option<TaskId>,
        now: DateTime<Utc>,
    ) -> EjaContext<'a> {
        self.project_with_workspaces(input, focus, now, &[])
    }

    /// The projection with the workspaces available this turn: the ids a task
    /// may choose. The fold itself keeps no configuration.
    #[must_use]
    pub fn project_with_workspaces<'a>(
        &'a self,
        input: &'a str,
        focus: Option<TaskId>,
        now: DateTime<Utc>,
        workspaces: &'a [WorkspaceGrant],
    ) -> EjaContext<'a> {
        let turn = self.turn();
        // The current input is separate from the conversation it belongs to.
        let history_before = self.events.last().map_or(0, |last| {
            last.turn.saturating_add(u64::from(last.kind == Kind::Eja))
        });
        let evidence = self.evidence();
        EjaContext {
            now: now.timestamp(),
            input,
            workspaces,
            history: self
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        Kind::User
                            | Kind::Eja
                            | Kind::Returned
                            | Kind::Departed
                            | Kind::Alone
                            | Kind::Approval
                            | Kind::Denial
                    )
                })
                .filter(|event| event.turn < history_before)
                .rev()
                .collect(),
            present_turns: &self.present_turns,
            goal: self.goal(),
            notes: self.notes(),
            identity: self.identity(),
            commitments: self.commitments(),
            relations: &self.edges,
            tasks: &self.tasks,
            focus: focus.filter(|id| self.knows(*id)),
            evidence: evidence.observations,
            unreadable_evidence: evidence.unreadable,
            proposals: self.pending_proposals(turn),
            refused: self.refusals(),
        }
    }

    fn open(&self, indices: &[usize]) -> Vec<&Event> {
        indices
            .iter()
            .filter_map(|index| self.events.get(*index))
            .filter(|event| !self.closed.contains(&event.id))
            .collect()
    }

    fn apply(&mut self, event: &Event) {
        let index = self.events.len();
        let host_approval = event.kind == Kind::Approval
            && event
                .data
                .as_ref()
                .and_then(|data| data.get("source"))
                .and_then(serde_json::Value::as_str)
                != Some("eja");
        if matches!(event.kind, Kind::User | Kind::Returned | Kind::Denial) || host_approval {
            self.present_turns.insert(event.turn);
        }
        if let Some(id) = task_id_in_scope(&event.scope) {
            self.last_task_id = self.last_task_id.max(id);
            if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                task.event_ids.push(event.id);
            }
        }
        if let Some(target) = event.closes {
            self.closed.insert(target);
        }
        match event.kind {
            Kind::Goal => {
                self.goals.push(index);
                self.resolve(event);
            }
            Kind::Commitment => {
                self.commitments.push(index);
                self.resolve(event);
            }
            Kind::Correction => {
                self.identity.push(index);
                self.resolve(event);
            }
            Kind::RecordDecision | Kind::Relationship => self.identity.push(index),
            Kind::Note => {
                // A note that closes another is a letting-go, not a memory.
                if event.closes.is_none() {
                    self.notes.push(index);
                }
            }
            Kind::ProposalConfirmed | Kind::ProposalRejected => self.resolve(event),
            Kind::Proposal => {
                if let Some(data) = decode::<ProposalData>(event) {
                    self.proposals.push(OpenProposal {
                        id: event.id,
                        data,
                        text: event.text.clone(),
                        at: event.at,
                        turn: event.turn,
                    });
                }
            }
            Kind::Approval => {
                if let Some(id) = proposal_of(event) {
                    self.approvals.push((event.turn, id));
                }
            }
            Kind::Denial => {
                if let Some(id) = proposal_of(event) {
                    self.denials.push(id);
                }
            }
            Kind::TaskOpened => self.opened(event),
            Kind::TaskAssigned => {
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    task.status = TaskStatus::Running;
                    task.assigned_at = Some(event.at);
                }
            }
            Kind::WorkerReport => {
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    let run = task.run.get_or_insert_with(Run::default);
                    run.digest = Some(event.text.clone());
                    task.reported_at = Some(event.at);
                    task.status = TaskStatus::Waiting;
                }
            }
            Kind::Observed => {
                let Some(data) = event.data.as_ref() else {
                    return;
                };
                let Some(observation) = Observation::read(data) else {
                    return;
                };
                if observation.kind == ObservationKind::Reconcile {
                    let target = task_id_in_scope(observation.target)
                        .or_else(|| task_id_in_scope(&event.scope));
                    if let Some(task) =
                        target.and_then(|id| self.tasks.iter_mut().find(|task| task.id == id))
                    {
                        let run = task.run.get_or_insert_with(Run::default);
                        run.reconcile = Some(observation.result.clone());
                    }
                    return;
                }
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                // Acceptance evidence is one observation the host made. An
                // observation that failed answers nothing.
                if observation.kind != ObservationKind::Witness || observation.error.is_some() {
                    return;
                }
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    let run = task.run.get_or_insert_with(Run::default);
                    run.evidence_id = Some(event.id);
                    run.evidence_result = Some(observation.result.clone());
                    run.witness_at = Some(event.at);
                }
            }
            Kind::TaskClosed => {
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                let Some(close) = decode::<CloseOutcome>(event) else {
                    return;
                };
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    task.closed_at = Some(event.at);
                    task.status = if close.accepted {
                        TaskStatus::Done
                    } else {
                        TaskStatus::Failed
                    };
                    task.close = Some(close);
                }
            }
            Kind::TaskRelation => self.relate(event),
            Kind::User
            | Kind::Eja
            | Kind::Refused
            | Kind::Failure
            | Kind::Returned
            | Kind::Departed
            | Kind::Alone => {}
        }
    }

    fn resolve(&mut self, event: &Event) {
        let Some(data) = event.data.as_ref() else {
            return;
        };
        let primary = data.get("proposal_id").and_then(serde_json::Value::as_u64);
        let correction = data
            .get("correction_proposal_id")
            .and_then(serde_json::Value::as_u64);
        if primary.is_none() && correction.is_none() {
            return;
        }
        self.proposals
            .retain(|proposal| Some(proposal.id) != primary && Some(proposal.id) != correction);
    }

    fn opened(&mut self, event: &Event) {
        let Some(id) = task_id_in_scope(&event.scope) else {
            return;
        };
        let Some(open) = decode::<TaskOpen>(event) else {
            return;
        };
        let task = Task {
            kind: open.kind,
            id,
            title: open.title.to_string(),
            request: open.request.to_string(),
            origin: open.origin.to_string(),
            acceptance: open.acceptance.to_string(),
            workspace: open.workspace.to_string(),
            status: TaskStatus::Open,
            created_at: event.at,
            assigned_at: None,
            reported_at: None,
            closed_at: None,
            close: None,
            event_ids: vec![event.id],
            // Only a task that executes carries a run; a group never does.
            run: (open.kind == TaskKind::Aja).then(|| Run::opened(open.evidence)),
        };
        let at = self
            .tasks
            .binary_search_by_key(&id, |task| task.id)
            .unwrap_or_else(|at| at);
        if self.tasks.get(at).is_some_and(|task| task.id == id) {
            return;
        }
        self.tasks.insert(at, task);
        if self.dangling.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.dangling);
        for edge in pending {
            if self.knows(edge.from) && self.knows(edge.to) {
                self.insert_edge(edge);
            } else {
                self.dangling.push(edge);
            }
        }
    }

    fn relate(&mut self, event: &Event) {
        let Some(edge) = decode::<Edge>(event) else {
            return;
        };
        // An edge is a fact even when it names a task this log cannot decode;
        // it enters the graph once both endpoints are known, never before.
        if self.knows(edge.from) && self.knows(edge.to) {
            self.insert_edge(edge);
        } else {
            self.dangling.push(edge);
        }
    }

    fn insert_edge(&mut self, edge: Edge) {
        let key = |edge: &Edge| (edge.from, edge.to, edge.kind);
        match self
            .edges
            .binary_search_by(|probe| key(probe).cmp(&key(&edge)))
        {
            Ok(_) => {}
            Err(at) => self.edges.insert(at, edge),
        }
    }
}
