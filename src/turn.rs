//! The single transition path: validate, record, execute, observe, project.
//!
//! A turn folds the journal once and pushes every event it commits into that
//! same fold, so a decision is validated against one derivation and no action
//! re-reads the log. Nothing here holds a second copy of a fact.
use crate::domain::{ProposalData, TaskOpen};
use crate::state::task_scope;
use crate::{
    now_utc, Action, Assignment, Brain, CloseOutcome, Control, ControlError, Edge, EventMeta, Host,
    Kind, Observation, ObservationKind, ProposalKind, RelationKind, Result, Selector, State, Store,
    Task, TaskId, TaskKind, TaskStatus, TurnEnd, TurnOptions, TurnOutcome, Worker,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

/// Commit one fact: the journal is the authority, and the fold is updated by the
/// same event, so no reader has to scan again.
fn commit(
    store: &Store,
    state: &mut State,
    kind: Kind,
    scope: &str,
    text: &str,
    turn: u64,
    meta: EventMeta,
) -> Result<()> {
    state.push(store.append(kind, scope, text, turn, meta)?);
    Ok(())
}

/// A refusal is information: it tells EJA what the state did not allow. A note is
/// told once per turn, so repeating an impossible action changes nothing.
fn refuse(store: &Store, state: &mut State, ctx: &ActionCtx<'_>, note: String) -> Result<bool> {
    if state.refusals(ctx.turn).contains(&note) {
        return Ok(false);
    }
    commit(
        store,
        state,
        Kind::Refused,
        "eja",
        &note,
        ctx.turn,
        EventMeta::new(ctx.at),
    )?;
    Ok(true)
}

/// Report tasks left Running by a previous process as unknown rather than
/// re-running them. Callers must already own the journal turn (a turn calls it
/// under its lease): this writes facts without taking one itself.
pub fn recover(store: &Store, turn: u64, at: i64) -> Result<Vec<TaskId>> {
    let mut state = State::load(store)?;
    recover_state(store, &mut state, turn, at)
}

fn recover_state(store: &Store, state: &mut State, turn: u64, at: i64) -> Result<Vec<TaskId>> {
    let running: Vec<TaskId> = state
        .tasks()
        .iter()
        .filter(|task| task.status == TaskStatus::Running)
        .map(|task| task.id)
        .collect();
    let mut recovered = Vec::new();
    for id in running {
        commit(
            store,
            state,
            Kind::WorkerReport,
            &task_scope(id),
            "worker session interrupted; state unknown",
            turn,
            EventMeta::new(at),
        )?;
        recovered.push(id);
    }
    Ok(recovered)
}

fn propose(
    store: &Store,
    state: &mut State,
    kind: ProposalKind,
    target_id: Option<u64>,
    text: &str,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    if !state.valid_proposal_target(kind, target_id) {
        return Ok(Some(format!(
            "proposal refused: no active {kind:?} target to replace"
        )));
    }
    commit(
        store,
        state,
        Kind::Proposal,
        "eja",
        text,
        turn,
        EventMeta::data(at, serde_json::to_value(ProposalData { kind, target_id })?),
    )?;
    Ok(None)
}

fn confirm_proposal(
    store: &Store,
    state: &mut State,
    proposal_id: u64,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let Some(proposal) = state
        .pending_proposals(turn)
        .into_iter()
        .find(|proposal| proposal.id == proposal_id)
    else {
        return Ok(Some(format!(
            "confirm refused: proposal {proposal_id} is not pending"
        )));
    };
    if proposal.turn >= turn {
        return Ok(Some(format!(
            "confirm refused: proposal {proposal_id} was created this turn; confirm it in a later turn"
        )));
    }
    if !state.approved(turn).contains(&proposal_id) {
        return Ok(Some(format!(
            "confirm refused: the host recorded no approval for proposal {proposal_id} in this turn"
        )));
    }
    if !state.valid_proposal_target(proposal.kind, proposal.target_id) {
        return Ok(Some(format!(
            "confirm refused: the target of proposal {proposal_id} is no longer active"
        )));
    }
    // One event both establishes the replacement and closes its predecessor.
    // A Correction has no replacement: it only withdraws its target.
    commit(
        store,
        state,
        proposal.kind.event_kind(),
        "eja",
        &proposal.text,
        turn,
        EventMeta {
            at,
            closes: proposal.target_id,
            data: Some(serde_json::json!({"proposal_id": proposal_id})),
        },
    )?;
    Ok(None)
}

fn reject_proposal(
    store: &Store,
    state: &mut State,
    proposal_id: u64,
    note: &str,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    if !state
        .pending_proposals(turn)
        .iter()
        .any(|proposal| proposal.id == proposal_id)
    {
        return Ok(Some(format!(
            "reject refused: proposal {proposal_id} is not pending"
        )));
    }
    commit(
        store,
        state,
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

fn append_identity(
    store: &Store,
    state: &mut State,
    action: &Action,
    turn: u64,
    at: i64,
) -> Result<()> {
    let (kind, text) = match action {
        Action::RecordDecision { text } => (Kind::RecordDecision, text),
        Action::RecordRelationship { text } => (Kind::Relationship, text),
        _ => return Ok(()),
    };
    commit(store, state, kind, "eja", text, turn, EventMeta::new(at))
}

fn open_task(ctx: &ActionCtx<'_>, state: &mut State, input: TaskOpen) -> Result<TaskId> {
    let id = state.next_task_id()?;
    commit(
        ctx.store,
        state,
        Kind::TaskOpened,
        &task_scope(id),
        &input.title,
        ctx.turn,
        EventMeta::data(ctx.at, serde_json::to_value(&input)?),
    )?;
    Ok(id)
}

fn link_tasks(
    store: &Store,
    state: &mut State,
    from: TaskId,
    to: TaskId,
    kind: RelationKind,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let Some(source) = state.task(from) else {
        return Ok(Some("relate refused: missing source task".into()));
    };
    let Some(target) = state.task(to) else {
        return Ok(Some("relate refused: missing target task".into()));
    };
    if from == to {
        return Ok(Some("relate refused: self relation".into()));
    }
    let mut edges = state.relations().to_vec();
    let edge = Edge { from, to, kind };
    if edges.contains(&edge) {
        return Ok(None);
    }
    match kind {
        RelationKind::PartOf => {
            if target.kind != TaskKind::Group || target.status != TaskStatus::Open {
                return Ok(Some(
                    "relate refused: containment needs an open group".into(),
                ));
            }
            if edges
                .iter()
                .any(|edge| edge.from == from && edge.kind == kind)
            {
                return Ok(Some(
                    "relate refused: a task already belongs to a group".into(),
                ));
            }
        }
        RelationKind::Continues | RelationKind::DependsOn => {
            if source.kind != TaskKind::Aja || source.status != TaskStatus::Open {
                return Ok(Some(
                    "relate refused: execution edges are fixed before dispatch".into(),
                ));
            }
            if kind == RelationKind::Continues
                && (target.kind != TaskKind::Aja || target.digest.is_none())
            {
                return Ok(Some(
                    "relate refused: continuation needs a completed aja record".into(),
                ));
            }
            if kind == RelationKind::Continues
                && edges
                    .iter()
                    .any(|edge| edge.from == from && edge.kind == kind)
            {
                return Ok(Some(
                    "relate refused: continuation already has a predecessor".into(),
                ));
            }
        }
        _ => {}
    }
    edges.push(edge);
    // A group completes after its members; a aja executes after its explicit
    // dependencies. One combined dependency graph catches cross-kind cycles.
    let dependency = |edge: &Edge| match edge.kind {
        RelationKind::PartOf => Some((edge.to, edge.from)),
        RelationKind::DependsOn | RelationKind::Continues => Some((edge.from, edge.to)),
        _ => None,
    };
    let (start, destination) = if kind == RelationKind::PartOf {
        (from, to)
    } else {
        (to, from)
    };
    if matches!(
        kind,
        RelationKind::PartOf | RelationKind::DependsOn | RelationKind::Continues
    ) {
        let mut pending = vec![start];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if id == destination {
                return Ok(Some("relate refused: dependency cycle".into()));
            }
            if seen.insert(id) {
                pending.extend(
                    edges
                        .iter()
                        .filter_map(dependency)
                        .filter(|(source, _)| *source == id)
                        .map(|(_, target)| target),
                );
            }
        }
    }
    commit(
        store,
        state,
        Kind::TaskRelation,
        &task_scope(from),
        "",
        turn,
        EventMeta::data(at, serde_json::to_value(Edge { from, to, kind })?),
    )?;
    Ok(None)
}

fn reserve_dispatch(
    ctx: &ActionCtx<'_>,
    state: &mut State,
    task_id: TaskId,
) -> Result<std::result::Result<Assignment, String>> {
    let Some(task) = state.task(task_id) else {
        return Ok(Err(format!("dispatch refused: no task {task_id}")));
    };
    if task.status != TaskStatus::Open {
        return Ok(Err(format!(
            "dispatch refused: task {task_id} is {}; only an Open task can be dispatched",
            task.status.as_str()
        )));
    }
    if task.kind != TaskKind::Aja {
        return Ok(Err(
            "dispatch refused: a group is not an execution aja".into()
        ));
    }
    for edge in state
        .relations()
        .iter()
        .filter(|edge| edge.from == task_id && edge.kind == RelationKind::DependsOn)
    {
        if !state
            .task(edge.to)
            .is_some_and(|other| other.status == TaskStatus::Done)
        {
            return Ok(Err(format!(
                "dispatch refused: dependency {} is not done",
                edge.to
            )));
        }
    }
    let predecessors: Vec<_> = state
        .relations()
        .iter()
        .filter(|edge| edge.from == task_id && edge.kind == RelationKind::Continues)
        .collect();
    if predecessors.len() > 1 {
        return Ok(Err(
            "dispatch refused: continuation has multiple predecessors".into(),
        ));
    }
    let resume = predecessors.first().map(|edge| edge.to);
    if resume.is_some_and(|id| {
        !state
            .task(id)
            .is_some_and(|other| other.kind == TaskKind::Aja && other.digest.is_some())
    }) {
        return Ok(Err(
            "dispatch refused: continuation needs a completed aja record".into(),
        ));
    }
    let assignment = Assignment {
        task_id: task.id,
        objective: task.request.clone(),
        input: task.origin.clone(),
        acceptance: task.acceptance.clone(),
        resume,
        issued_at: ctx.at,
    };
    commit(
        ctx.store,
        state,
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
    state: &mut State,
    out: &mut TurnOutcome,
    assignment: Assignment,
    digest: String,
) -> Result<()> {
    commit(
        ctx.store,
        state,
        Kind::WorkerReport,
        &task_scope(assignment.task_id),
        &digest,
        ctx.turn,
        EventMeta::new(ctx.at),
    )?;
    out.dispatched.push(assignment.task_id);
    Ok(())
}

/// One execution owner per world. Reports are recorded in dispatch order, so the
/// journal never depends on thread timing; with more than one slot the worlds run
/// at once, and each completed run leaves its evidence before the next report.
fn run_dispatches(
    ctx: &ActionCtx<'_>,
    state: &mut State,
    out: &mut TurnOutcome,
    pending: Vec<Assignment>,
) -> Result<()> {
    let slots = ctx.options.limits.slots.max(1);
    if slots == 1 || pending.len() <= 1 {
        for assignment in pending {
            ctx.control.check()?;
            let task_id = assignment.task_id;
            let started = Instant::now();
            let digest = run_one(ctx.worker, &assignment, ctx.control);
            out.worker_ms += started.elapsed().as_millis();
            record_dispatch(ctx, state, out, assignment, digest)?;
            observe_evidence(ctx, state, task_id)?;
        }
        return Ok(());
    }
    let started = Instant::now();
    let worker = ctx.worker;
    let control = ctx.control.clone();
    let queue = std::sync::Mutex::new(
        pending
            .into_iter()
            .enumerate()
            .collect::<std::collections::VecDeque<_>>(),
    );
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..slots {
            let queue = &queue;
            let results = &results;
            let control = &control;
            scope.spawn(move || loop {
                let next = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                let Some((index, assignment)) = next else {
                    break;
                };
                let digest = run_one(worker, &assignment, control);
                if let Ok(mut results) = results.lock() {
                    results.push((index, assignment, digest));
                }
            });
        }
    });
    out.worker_ms += started.elapsed().as_millis();
    let mut collected = results
        .into_inner()
        .map_err(|_| "dispatch results are unavailable")?;
    collected.sort_by_key(|(index, _, _)| *index);
    for (_, assignment, digest) in collected {
        ctx.control.check()?;
        let task_id = assignment.task_id;
        record_dispatch(ctx, state, out, assignment, digest)?;
        observe_evidence(ctx, state, task_id)?;
    }
    Ok(())
}

fn run_one(worker: &dyn Worker, assignment: &Assignment, control: &Control) -> String {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        worker.execute(assignment, control)
    }))
    .unwrap_or_else(|_| "aja panicked".to_string())
}

/// The host's own observation of what a run achieved. Failure is recorded too: a
/// witness that could not answer leaves the judgment undecidable, and says why.
fn observe_evidence(ctx: &ActionCtx<'_>, state: &mut State, task_id: TaskId) -> Result<()> {
    let Some(task) = state.task(task_id) else {
        return Ok(());
    };
    let Some(request) = task.evidence.clone() else {
        return Ok(());
    };
    if task.evidence_id.is_some() {
        return Ok(());
    }
    let target = request["tool"].as_str().unwrap_or("witness").to_string();
    let observation = match ctx.host.witness(&request, ctx.control) {
        Ok(result) => Observation {
            kind: ObservationKind::Witness,
            target,
            arguments: request,
            selector: Selector::Whole,
            result,
            error: None,
        },
        Err(error) => Observation {
            kind: ObservationKind::Witness,
            target,
            arguments: request,
            selector: Selector::Whole,
            result: serde_json::Value::Null,
            error: Some(error.to_string()),
        },
    };
    commit(
        ctx.store,
        state,
        Kind::Observed,
        &task_scope(task_id),
        "",
        ctx.turn,
        EventMeta::data(ctx.at, serde_json::to_value(&observation)?),
    )
}

/// Reconciling a world is the host's act and its account is evidence. Conflicts
/// are recorded, never merged away.
fn reconcile(
    ctx: &ActionCtx<'_>,
    state: &mut State,
    task_id: TaskId,
) -> Result<std::result::Result<(), String>> {
    let Some(task) = state.task(task_id) else {
        return Ok(Err(format!("reconcile refused: no task {task_id}")));
    };
    if task.digest.is_none() {
        return Ok(Err(format!(
            "reconcile refused: task {task_id} has no run to reconcile"
        )));
    }
    let observation = match ctx.host.reconcile(task_id, ctx.control) {
        Ok(result) => Observation {
            kind: ObservationKind::Reconcile,
            target: task_scope(task_id),
            arguments: serde_json::json!({"task": task_id}),
            selector: Selector::Whole,
            result,
            error: None,
        },
        Err(error) => return Ok(Err(format!("reconcile refused: {error}"))),
    };
    commit(
        ctx.store,
        state,
        Kind::Observed,
        "eja",
        "",
        ctx.turn,
        EventMeta::data(ctx.at, serde_json::to_value(&observation)?),
    )?;
    Ok(Ok(()))
}

fn close_task(
    store: &Store,
    state: &mut State,
    task_id: TaskId,
    accepted: bool,
    note: &str,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let Some(task) = state.task(task_id) else {
        return Ok(Some(format!("close refused: no task {task_id}")));
    };
    if task.kind == TaskKind::Group {
        if task.status != TaskStatus::Open {
            return Ok(Some("close refused: group is already closed".into()));
        }
        let children: Vec<&Task> = state
            .relations()
            .iter()
            .filter(|edge| edge.to == task_id && edge.kind == RelationKind::PartOf)
            .filter_map(|edge| state.task(edge.from))
            .collect();
        if children.iter().any(|child| child.status.is_active()) {
            return Ok(Some(
                "close refused: a group still has active children".into(),
            ));
        }
    } else if task.status != TaskStatus::Waiting && !(task.status == TaskStatus::Open && !accepted)
    {
        return Ok(Some(format!(
            "close refused: task {task_id} is {}; a aja record is required first",
            task.status.as_str()
        )));
    }
    // Acceptance needs the host's observation, not the runner's word. A
    // rejection needs none: the point of the gate is that EJA may not accept
    // what it cannot verify, not that it must wait forever for a witness that
    // never answers.
    if accepted && task.evidence.is_some() && task.evidence_id.is_none() {
        return Ok(Some(format!(
            "close refused: accepting task {task_id} needs the evidence its contract asked for and the host observed none; rejecting needs no evidence"
        )));
    }
    let data = serde_json::to_value(CloseOutcome {
        accepted,
        note: note.to_string(),
    })?;
    commit(
        store,
        state,
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
    host: &'a dyn Host,
    control: &'a Control,
    options: &'a TurnOptions,
}

fn apply_actions(
    actions: &[Action],
    ctx: &ActionCtx<'_>,
    state: &mut State,
    out: &mut TurnOutcome,
    focus: &mut Option<TaskId>,
) -> Result<bool> {
    let mut acted = false;
    let mut told = false;
    let mut pending: Vec<Assignment> = Vec::new();
    for action in actions {
        ctx.control.check()?;
        match action {
            Action::RecordDecision { .. } | Action::RecordRelationship { .. } => {
                append_identity(ctx.store, state, action, ctx.turn, ctx.at)?
            }
            Action::Propose {
                kind,
                target_id,
                text,
            } => {
                if let Some(note) =
                    propose(ctx.store, state, *kind, *target_id, text, ctx.turn, ctx.at)?
                {
                    told |= refuse(ctx.store, state, ctx, note)?;
                } else {
                    acted = true;
                }
            }
            Action::Confirm { proposal_id } => {
                if let Some(note) =
                    confirm_proposal(ctx.store, state, *proposal_id, ctx.turn, ctx.at)?
                {
                    told |= refuse(ctx.store, state, ctx, note)?;
                }
            }
            Action::Reject { proposal_id, note } => {
                if let Some(note) =
                    reject_proposal(ctx.store, state, *proposal_id, note, ctx.turn, ctx.at)?
                {
                    told |= refuse(ctx.store, state, ctx, note)?;
                }
            }
            Action::OpenGroup { title, acceptance } => {
                let id = open_task(
                    ctx,
                    state,
                    TaskOpen {
                        kind: TaskKind::Group,
                        title: title.clone(),
                        request: String::new(),
                        origin: ctx.input.into(),
                        acceptance: acceptance.clone(),
                        evidence: None,
                    },
                )?;
                *focus = Some(id);
                acted = true;
            }
            Action::OpenTask {
                title,
                request,
                acceptance,
                evidence,
                run,
            } => {
                let id = open_task(
                    ctx,
                    state,
                    TaskOpen {
                        kind: TaskKind::Aja,
                        title: title.clone(),
                        request: request.clone(),
                        origin: ctx.input.into(),
                        acceptance: acceptance.clone(),
                        evidence: evidence.clone(),
                    },
                )?;
                *focus = Some(id);
                acted = true;
                if *run {
                    match reserve_dispatch(ctx, state, id)? {
                        Ok(assignment) => pending.push(assignment),
                        Err(note) => told |= refuse(ctx.store, state, ctx, note)?,
                    }
                }
            }
            Action::Dispatch { task_id } => match reserve_dispatch(ctx, state, *task_id)? {
                Ok(assignment) => {
                    *focus = Some(*task_id);
                    pending.push(assignment);
                    acted = true;
                }
                Err(note) => told |= refuse(ctx.store, state, ctx, note)?,
            },
            Action::LinkTasks { from, to, kind } => {
                if let Some(note) =
                    link_tasks(ctx.store, state, *from, *to, *kind, ctx.turn, ctx.at)?
                {
                    told |= refuse(ctx.store, state, ctx, note)?;
                }
            }
            Action::CloseTask {
                task_id,
                accepted,
                note,
            } => {
                if let Some(reason) = close_task(
                    ctx.store, state, *task_id, *accepted, note, ctx.turn, ctx.at,
                )? {
                    told |= refuse(ctx.store, state, ctx, reason)?;
                }
            }
            Action::Reconcile { task_id } => match reconcile(ctx, state, *task_id)? {
                Ok(()) => acted = true,
                Err(note) => told |= refuse(ctx.store, state, ctx, note)?,
            },
            Action::Focus { task_id } => {
                if !state.knows(*task_id) {
                    told |= refuse(
                        ctx.store,
                        state,
                        ctx,
                        format!("focus refused: no task {task_id}"),
                    )?;
                    continue;
                }
                *focus = Some(*task_id);
                acted = true;
            }
            Action::Look {
                target,
                arguments,
                selector,
            } => {
                let (result, error) =
                    match ctx
                        .host
                        .look(target, arguments, selector, state.events(), ctx.control)
                    {
                        Ok(value) if crate::measure::bytes(&value)? <= crate::aja::FRAME_LIMIT => {
                            (value, None)
                        }
                        Ok(_) => (
                            Value::Null,
                            Some(
                                "look output exceeds its byte budget; narrow the requested view"
                                    .into(),
                            ),
                        ),
                        Err(error) => (Value::Null, Some(error.to_string())),
                    };
                let observation = Observation {
                    kind: ObservationKind::Look,
                    target: target.clone(),
                    arguments: arguments.clone(),
                    selector: selector.clone(),
                    result,
                    error,
                };
                commit(
                    ctx.store,
                    state,
                    Kind::Observed,
                    "eja",
                    "",
                    ctx.turn,
                    EventMeta::data(ctx.at, serde_json::to_value(&observation)?),
                )?;
                acted = true;
            }
        }
    }
    run_dispatches(ctx, state, out, pending)?;
    Ok(acted || told)
}

pub fn turn(
    store: &Store,
    input: &str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    host: &dyn Host,
) -> Result<TurnOutcome> {
    turn_at(store, input, brain, worker, host, now_utc())
}

pub fn turn_at(
    store: &Store,
    input: &str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    host: &dyn Host,
    now: DateTime<Utc>,
) -> Result<TurnOutcome> {
    turn_at_with_options(
        store,
        input,
        brain,
        worker,
        host,
        now,
        &TurnOptions::default(),
    )
}

fn stop(out: &mut TurnOutcome, error: ControlError) {
    out.end = match error {
        ControlError::Cancelled => TurnEnd::Cancelled,
        ControlError::Deadline => TurnEnd::Budget,
    };
    out.reason = Some(error.to_string());
}

pub fn turn_at_with_options(
    store: &Store,
    input: &str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    host: &dyn Host,
    now: DateTime<Utc>,
    options: &TurnOptions,
) -> Result<TurnOutcome> {
    let _journal_lease = store.lock_turn()?;
    options.limits.validate()?;
    let control = Control::new(
        Duration::from_millis(options.limits.timeout_ms),
        options.cancelled.clone(),
    );
    let at = now.timestamp();
    let turn_no = store.next_turn()?;
    // One read of the journal, then every commit updates the same fold: an
    // action is validated against the state it will produce, never a re-scan.
    let mut state = State::load(store)?;
    recover_state(store, &mut state, turn_no, at)?;
    if options.focus.is_some_and(|id| !state.knows(id)) {
        return Err("focus names an unknown task".into());
    }
    commit(
        store,
        &mut state,
        Kind::User,
        "eja",
        input,
        turn_no,
        EventMeta::new(at),
    )?;
    // An approval is a fact, not a caller's flag: it is recorded once, with the
    // turn it authorizes, and every later read comes from the journal.
    for proposal_id in &options.approvals {
        commit(
            store,
            &mut state,
            Kind::Approval,
            "eja",
            "",
            turn_no,
            EventMeta::data(
                at,
                serde_json::json!({"proposal_id": proposal_id, "source": "host"}),
            ),
        )?;
    }

    let active: Vec<&Task> = state
        .tasks()
        .iter()
        .filter(|task| task.status.is_active())
        .collect();
    let mut focus = options.focus.or_else(|| {
        if active.len() == 1 && active[0].status == TaskStatus::Waiting {
            Some(active[0].id)
        } else {
            None
        }
    });
    let mut out = TurnOutcome {
        reply: String::new(),
        context: state.project(input, focus, now),
        dispatched: Vec::new(),
        decisions: 0,
        worker_ms: 0,
        end: TurnEnd::Complete,
        reason: None,
    };
    let ctx = ActionCtx {
        store,
        input,
        turn: turn_no,
        at,
        worker,
        host,
        control: &control,
        options,
    };
    let mut actions_used = 0usize;
    loop {
        if let Err(error) = control.check() {
            stop(&mut out, error);
            break;
        }
        if out.decisions >= options.limits.decisions {
            out.end = TurnEnd::Budget;
            out.reason = Some("turn decision budget exhausted".into());
            break;
        }
        out.decisions += 1;
        let decision = brain.decide(&out.context, &control);
        if let Err(error) = control.check() {
            stop(&mut out, error);
            break;
        }
        if decision.actions.len() > options.limits.actions.saturating_sub(actions_used) {
            out.end = TurnEnd::Budget;
            out.reason = Some("turn action budget exhausted".into());
            break;
        }
        let dispatches = decision
            .actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    Action::Dispatch { .. } | Action::OpenTask { run: true, .. }
                )
            })
            .count();
        if dispatches
            > options
                .limits
                .dispatches
                .saturating_sub(out.dispatched.len())
        {
            out.end = TurnEnd::Budget;
            out.reason = Some("turn dispatch budget exhausted".into());
            break;
        }
        actions_used += decision.actions.len();
        for note in &decision.refused {
            refuse(store, &mut state, &ctx, note.clone())?;
        }
        let acted = match apply_actions(&decision.actions, &ctx, &mut state, &mut out, &mut focus) {
            Ok(acted) => acted,
            Err(error) => {
                if let Err(stopped) = control.check() {
                    stop(&mut out, stopped);
                    break;
                }
                return Err(error);
            }
        };
        // The projection is what the model just read. If the facts this decision
        // committed leave it identical, they added no information and the turn is
        // at its fixpoint; a decision that committed nothing is a final reply.
        let next = state.project(input, focus, now);
        let repeated = next == out.context;
        out.context = next;
        if !acted {
            out.reply = decision.reply.trim().to_string();
            break;
        }
        if repeated {
            out.end = TurnEnd::Stalled;
            break;
        }
    }
    let mut meta = EventMeta::new(at);
    if out.end != TurnEnd::Complete {
        out.reply = format!(
            "[turn {:?}] {}",
            out.end,
            out.reason.as_deref().unwrap_or("projection repeated")
        );
        meta.data = Some(serde_json::json!({"end": out.end, "reason": out.reason}));
    }
    recover_state(store, &mut state, turn_no, at)?;
    commit(
        store,
        &mut state,
        Kind::Eja,
        "eja",
        &out.reply,
        turn_no,
        meta,
    )?;
    out.context = state.project(input, focus, now);
    Ok(out)
}
