//! `S = fold(L)`: the one value derived from committed facts.
//!
//! A turn folds the journal once and pushes every event it commits into the same
//! value, so validation and projection read one derivation instead of scanning
//! the log again per action. Nothing here executes an effect, and a projection
//! never changes a fact.

use crate::domain::{ProposalData, TaskOpen};
use crate::{
    CloseOutcome, Edge, EjaContext, Event, Kind, Observation, ObservationKind, ProposalKind,
    ProposalView, Result, Store, Task, TaskId, TaskStatus,
};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

/// A proposal that no event has resolved yet. Its own event id is its identity.
#[derive(Debug, Clone)]
struct OpenProposal {
    id: u64,
    data: ProposalData,
    text: String,
    at: i64,
    turn: u64,
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
    /// Tasks by id.
    tasks: Vec<Task>,
    /// Execution and containment edges, sorted and unique.
    edges: Vec<Edge>,
    /// Edges that named a task the log had not opened yet.
    dangling: Vec<Edge>,
    proposals: Vec<OpenProposal>,
    /// `(turn, proposal)` pairs the host approved.
    approvals: Vec<(u64, u64)>,
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

pub(crate) fn task_scope(task_id: TaskId) -> String {
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

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The newest committed fact identifies everything committed so far.
    pub fn last_id(&self) -> u64 {
        self.events.last().map_or(0, |event| event.id)
    }

    /// The turn the newest fact belongs to.
    pub fn turn(&self) -> u64 {
        self.events.last().map_or(0, |event| event.turn)
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn knows(&self, id: TaskId) -> bool {
        self.task(id).is_some()
    }

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
    pub fn goal(&self) -> Option<&Event> {
        self.goals
            .iter()
            .rev()
            .filter_map(|index| self.events.get(*index))
            .find(|event| !self.closed.contains(&event.id))
    }

    pub fn commitments(&self) -> Vec<&Event> {
        self.open(&self.commitments)
    }

    pub fn identity(&self) -> Vec<&Event> {
        self.identity
            .iter()
            .filter_map(|index| self.events.get(*index))
            .collect()
    }

    /// Actions this turn that the host could not carry out, from EJA's own words.
    ///
    /// A turn's facts are the journal's tail, so the scan stops at its first
    /// event: reading this turn costs this turn, not the whole log.
    pub fn refusals(&self, turn: u64) -> Vec<&str> {
        debug_assert_eq!(turn, self.turn(), "a turn reads only its own events");
        self.events
            .iter()
            .rev()
            .take_while(|event| event.turn == turn)
            .filter(|event| event.kind == Kind::Refused)
            .map(|event| event.text.as_str())
            .collect()
    }

    /// What this turn observed, one entry per distinct observation: a repeated
    /// read keeps the newest answer, so the view states what is currently true.
    pub fn evidence(&self, turn: u64) -> Vec<Observation<'_>> {
        debug_assert_eq!(turn, self.turn(), "a turn reads only its own events");
        let mut out: Vec<Observation<'_>> = Vec::new();
        // Walk the tail backwards, then restore the order the turn happened in.
        for event in self
            .events
            .iter()
            .rev()
            .take_while(|event| event.turn == turn)
            .filter(|event| event.kind == Kind::Observed)
        {
            let Some(data) = event.data.as_ref() else {
                continue;
            };
            let Some(observation) = Observation::read(data) else {
                continue;
            };
            // A repeated read is one entry: the newest answer is what holds.
            if out.iter().any(|other| other.same_subject(&observation)) {
                continue;
            }
            out.push(observation);
        }
        out.reverse();
        out
    }

    /// Approvals the host recorded for one turn.
    pub fn approved(&self, turn: u64) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .approvals
            .iter()
            .filter(|(approved_in, _)| *approved_in == turn)
            .map(|(_, proposal)| *proposal)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

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
            })
            .collect()
    }

    /// A proposal may only name an active identity fact of its own kind: a
    /// correction withdraws its target, a goal or commitment replaces it.
    pub fn valid_proposal_target(&self, kind: ProposalKind, target_id: Option<u64>) -> bool {
        let Some(target_id) = target_id else {
            return kind != ProposalKind::Correction;
        };
        !self.closed.contains(&target_id)
            && self.events.iter().any(|event| {
                event.id == target_id
                    && event.kind.is_closable()
                    && (kind == ProposalKind::Correction || event.kind == kind.event_kind())
            })
    }

    /// The model's bounded working view: the projection of this state. It
    /// borrows every fact it shows, so a decision costs no copy of the log.
    pub fn project<'a>(
        &'a self,
        input: &'a str,
        focus: Option<TaskId>,
        now: DateTime<Utc>,
    ) -> EjaContext<'a> {
        let turn = self.turn();
        // The current input is separate from the conversation it belongs to.
        let history_before = self.events.last().map_or(0, |last| {
            last.turn.saturating_add(u64::from(last.kind == Kind::Eja))
        });
        EjaContext {
            now: now.timestamp(),
            input,
            history: self
                .events
                .iter()
                .filter(|event| matches!(event.kind, Kind::User | Kind::Eja))
                .filter(|event| event.turn < history_before)
                .rev()
                .collect(),
            goal: self.goal(),
            identity: self.identity(),
            commitments: self.commitments(),
            relations: &self.edges,
            tasks: &self.tasks,
            focus: focus.filter(|id| self.knows(*id)),
            evidence: self.evidence(turn),
            proposals: self.pending_proposals(turn),
            refused: self.refusals(turn),
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
        if let Some(id) = task_id_in_scope(&event.scope) {
            self.last_task_id = self.last_task_id.max(id);
        }
        if event.kind.is_closable() || event.kind == Kind::Correction {
            if let Some(target) = event.closes {
                self.closed.insert(target);
            }
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
            Kind::TaskOpened => self.opened(event),
            Kind::TaskAssigned => {
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                if event.data.is_none() {
                    return;
                }
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    task.status = TaskStatus::Running;
                }
            }
            Kind::WorkerReport => {
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    task.digest = Some(event.text.clone());
                    task.digest_at = Some(event.at);
                    task.status = TaskStatus::Waiting;
                }
            }
            Kind::Observed => {
                let Some(id) = task_id_in_scope(&event.scope) else {
                    return;
                };
                let Some(data) = event.data.as_ref() else {
                    return;
                };
                let Some(observation) = Observation::read(data) else {
                    return;
                };
                // Acceptance evidence is one observation the host made. An
                // observation that failed answers nothing.
                if observation.kind != ObservationKind::Witness || observation.error.is_some() {
                    return;
                }
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                    task.evidence_id = Some(event.id);
                    task.evidence_result = Some(observation.result.clone());
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
            Kind::User | Kind::Eja | Kind::Refused => {}
        }
    }

    fn resolve(&mut self, event: &Event) {
        if let Some(id) = proposal_of(event) {
            self.proposals.retain(|proposal| proposal.id != id);
        }
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
            status: TaskStatus::Open,
            created_at: event.at,
            digest_at: None,
            closed_at: None,
            digest: None,
            close: None,
            evidence: open.evidence,
            evidence_id: None,
            evidence_result: None,
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
