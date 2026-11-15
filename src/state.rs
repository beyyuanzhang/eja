//! Deterministic projections of committed facts; no effect execution.
use crate::domain::{ProposalData, TaskOpen};
use crate::*;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashSet;

pub(crate) fn closed_ids(events: &[Event]) -> HashSet<u64> {
    events
        .iter()
        .filter(|event| event.kind.is_closable() || event.kind == Kind::Correction)
        .filter_map(|event| event.closes)
        .collect()
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
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                Kind::Correction | Kind::RecordDecision | Kind::Relationship
            )
        })
        .cloned()
        .collect()
}

pub(crate) fn task_scope(task_id: TaskId) -> String {
    format!("task:{task_id}")
}

fn task_id_in_scope(scope: &str) -> Option<TaskId> {
    scope.strip_prefix("task:")?.parse().ok()
}

pub(crate) fn next_task_id(events: &[Event]) -> Result<TaskId> {
    events
        .iter()
        .filter(|event| event.kind == Kind::TaskOpened)
        .filter_map(|event| task_id_in_scope(&event.scope))
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "task id space exhausted".into())
}

pub fn relations(events: &[Event]) -> Vec<Edge> {
    relations_with_tasks(events, &tasks(events))
}

pub(crate) fn relations_with_tasks(events: &[Event], tasks: &[Task]) -> Vec<Edge> {
    let known: std::collections::HashSet<TaskId> = tasks.iter().map(|task| task.id).collect();
    let mut out = Vec::new();
    for event in events {
        if event.kind != Kind::TaskRelation {
            continue;
        }
        if let Some(data) = event
            .data
            .as_ref()
            .and_then(|value| serde_json::from_value::<Edge>(value.clone()).ok())
        {
            if known.contains(&data.from) && known.contains(&data.to) {
                out.push(data);
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
        let Some(task_id) = task_id_in_scope(&event.scope) else {
            continue;
        };
        match event.kind {
            Kind::TaskOpened => {
                let Some(open) = event
                    .data
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<TaskOpen>(value.clone()).ok())
                else {
                    continue;
                };
                out.push(Task {
                    kind: open.kind,
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
                    witness: open.evidence.clone(),
                    witness_id: None,
                    witness_result: None,
                });
            }
            Kind::TaskAssigned => {
                if event.data.is_some() {
                    if let Some(task) = out.iter_mut().find(|task| task.id == task_id) {
                        task.status = TaskStatus::Running;
                    }
                }
            }
            Kind::WorkerReport => {
                if let Some(task) = out.iter_mut().find(|task| task.id == task_id) {
                    task.digest = Some(event.text.clone());
                    task.digest_at = Some(event.at);
                    task.status = TaskStatus::Waiting;
                }
            }
            Kind::Observed => {
                let Some(task) = out.iter_mut().find(|task| task.id == task_id) else {
                    continue;
                };
                let Some(data) = event.data.as_ref() else {
                    continue;
                };
                let Ok(observation) = serde_json::from_value::<Observation>(data.clone()) else {
                    continue;
                };
                // Acceptance evidence is one observation the host made, scoped to
                // the task it answers. An observation that failed is not evidence.
                if observation.target == "witness" && observation.error.is_none() {
                    task.witness_id = Some(event.id);
                    task.witness_result = Some(observation.result);
                }
            }
            Kind::TaskClosed => {
                let Some(close) = event
                    .data
                    .as_ref()
                    .and_then(|value| serde_json::from_value::<CloseOutcome>(value.clone()).ok())
                else {
                    continue;
                };
                if let Some(task) = out.iter_mut().find(|task| task.id == task_id) {
                    task.closed_at = Some(event.at);
                    task.status = if close.accepted {
                        TaskStatus::Done
                    } else {
                        TaskStatus::Failed
                    };
                    task.close = Some(close);
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|task| task.id);
    out
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
    let focus = focus_task.filter(|id| all_tasks.iter().any(|task| task.id == *id));
    let history_before = events.last().map_or(0, |last| {
        last.turn.saturating_add(u64::from(last.kind == Kind::Eja))
    });
    let turn = events.last().map_or(0, |last| last.turn);
    Ok(EjaContext {
        now: now.timestamp(),
        input: input.to_string(),
        history: events
            .iter()
            .filter(|event| matches!(event.kind, Kind::User | Kind::Eja))
            .filter(|event| event.turn < history_before)
            .rev()
            .cloned()
            .collect(),
        goal: goal_event(&events).cloned(),
        identity: active_identity(&events),
        commitments: open_commitments(&events),
        relations: relations_with_tasks(&events, &all_tasks),
        tasks: all_tasks,
        focus,
        evidence: evidence.to_vec(),
        proposals: pending_proposals(&events, turn),
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

/// Approvals the host recorded for one turn, read back from the journal.
pub(crate) fn approvals(events: &[Event], turn: u64) -> Vec<u64> {
    let mut out: Vec<u64> = events
        .iter()
        .filter(|event| event.kind == Kind::Approval && event.turn == turn)
        .filter_map(|event| {
            event
                .data
                .as_ref()
                .and_then(|data| data.get("proposal_id"))
                .and_then(Value::as_u64)
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub(crate) fn pending_proposals(events: &[Event], turn: u64) -> Vec<ProposalView> {
    let approved = approvals(events, turn);
    let resolved: HashSet<u64> = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                Kind::ProposalConfirmed
                    | Kind::ProposalRejected
                    | Kind::Goal
                    | Kind::Commitment
                    | Kind::Correction
            )
        })
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
                approved: approved.contains(&event.id),
            })
        })
        .collect()
}

pub(crate) fn valid_proposal_target(
    events: &[Event],
    kind: ProposalKind,
    target_id: Option<u64>,
) -> bool {
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
