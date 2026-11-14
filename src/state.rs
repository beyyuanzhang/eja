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
    let focus = focus_task.and_then(|id| all_tasks.iter().find(|task| task.id == id).cloned());
    let mut summaries: Vec<TaskSummary> = all_tasks
        .iter()
        .map(|task| TaskSummary {
            kind: task.kind,
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
        approved_proposals: events
            .iter()
            .rev()
            .find(|event| event.kind == Kind::User)
            .and_then(|event| event.data.as_ref())
            .and_then(|data| data.get("approved_proposals"))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default(),
        history: events
            .iter()
            .filter(|event| matches!(event.kind, Kind::User | Kind::Eja))
            .filter(|event| {
                event.turn
                    < events
                        .last()
                        .map_or(0, |last| last.turn + u64::from(last.kind == Kind::Eja))
            })
            .rev()
            .cloned()
            .collect(),
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

pub(crate) fn pending_proposals(events: &[Event]) -> Vec<ProposalView> {
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
            })
        })
        .collect()
}

pub(crate) fn proposal_event(events: &[Event], proposal_id: u64) -> Option<Event> {
    events
        .iter()
        .find(|event| event.id == proposal_id && event.kind == Kind::Proposal)
        .cloned()
}

pub(crate) fn proposal_is_pending(events: &[Event], proposal_id: u64) -> bool {
    pending_proposals(events)
        .iter()
        .any(|proposal| proposal.id == proposal_id)
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
