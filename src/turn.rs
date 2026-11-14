//! The single transition path: validate, record, execute, observe, project.
use crate::domain::{ProposalData, TaskOpen};
use crate::state::{
    next_task_id, proposal_event, proposal_is_pending, relations_with_tasks, task_scope,
    valid_proposal_target,
};
use crate::*;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

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
            data: Some(serde_json::json!({"proposal_id": proposal_id})),
        },
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
    kind: TaskKind,
    title: &'a str,
    request: &'a str,
    origin: &'a str,
    acceptance: &'a str,
    turn: u64,
    at: i64,
}

fn open_task(store: &Store, input: OpenTaskInput<'_>) -> Result<TaskId> {
    let id = next_task_id(&store.events()?)?;
    let data = serde_json::to_value(TaskOpen {
        kind: input.kind,
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

fn link_tasks(
    store: &Store,
    from: TaskId,
    to: TaskId,
    kind: RelationKind,
    turn: u64,
    at: i64,
) -> Result<Option<String>> {
    let events = store.events()?;
    let all = tasks(&events);
    let Some(source) = all.iter().find(|task| task.id == from) else {
        return Ok(Some("relate refused: missing source task".into()));
    };
    let Some(target) = all.iter().find(|task| task.id == to) else {
        return Ok(Some("relate refused: missing target task".into()));
    };
    if from == to {
        return Ok(Some("relate refused: self relation".into()));
    }
    let mut edges = relations_with_tasks(&events, &all);
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
            if source.kind != TaskKind::Node || source.status != TaskStatus::Open {
                return Ok(Some(
                    "relate refused: execution edges are fixed before dispatch".into(),
                ));
            }
            if kind == RelationKind::Continues
                && (target.kind != TaskKind::Node || target.digest.is_none())
            {
                return Ok(Some(
                    "relate refused: continuation needs a completed node record".into(),
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
    // A group completes after its members; a node executes after its explicit
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
    store.append(
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
            task.status.as_str()
        )));
    }
    if task.kind != TaskKind::Node {
        return Ok(Err(
            "dispatch refused: a group is not an execution node".into()
        ));
    }
    let events = ctx.store.events()?;
    let all = tasks(&events);
    let edges = relations_with_tasks(&events, &all);
    for edge in edges
        .iter()
        .filter(|edge| edge.from == task_id && edge.kind == RelationKind::DependsOn)
    {
        if !all
            .iter()
            .any(|other| other.id == edge.to && other.status == TaskStatus::Done)
        {
            return Ok(Err(format!(
                "dispatch refused: dependency {} is not done",
                edge.to
            )));
        }
    }
    let predecessors: Vec<_> = edges
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
        !all.iter()
            .any(|other| other.id == id && other.kind == TaskKind::Node && other.digest.is_some())
    }) {
        return Ok(Err(
            "dispatch refused: continuation needs a completed node record".into(),
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
    let capacity = ctx.worker.capacity().max(1);
    for group in pending.chunks(capacity) {
        ctx.control.check()?;
        let started = Instant::now();
        let digests = if group.len() == 1 {
            vec![std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ctx.worker.execute_controlled(&group[0], ctx.control)
            }))
            .unwrap_or_else(|_| "node panicked".to_string())]
        } else {
            let mut digests = Vec::with_capacity(group.len());
            let worker = ctx.worker;
            let control = ctx.control;
            std::thread::scope(|scope| {
                let handles: Vec<_> = group
                    .iter()
                    .map(|assignment| {
                        scope.spawn(move || worker.execute_controlled(assignment, control))
                    })
                    .collect();
                for handle in handles {
                    digests.push(
                        handle
                            .join()
                            .unwrap_or_else(|_| "node panicked".to_string()),
                    );
                }
            });
            digests
        };
        out.worker_ms += started.elapsed().as_millis();
        // Publish completed runs before checking the next chunk's authority.
        for (assignment, digest) in group.iter().cloned().zip(digests) {
            record_dispatch(ctx, out, assignment, digest)?;
        }
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
    if task.kind == TaskKind::Group {
        if task.status != TaskStatus::Open {
            return Ok(Some("close refused: group is already closed".into()));
        }
        let events = store.events()?;
        let all = tasks(&events);
        let edges = relations_with_tasks(&events, &all);
        let children: Vec<_> = edges
            .iter()
            .filter(|edge| edge.to == task_id && edge.kind == RelationKind::PartOf)
            .filter_map(|edge| all.iter().find(|child| child.id == edge.from))
            .collect();
        if children.iter().any(|child| child.status.is_active()) {
            return Ok(Some(
                "close refused: a group still has active children".into(),
            ));
        }
    } else if task.status != TaskStatus::Waiting && !(task.status == TaskStatus::Open && !accepted)
    {
        return Ok(Some(format!(
            "close refused: task {task_id} is {}; a node record is required first",
            task.status.as_str()
        )));
    }
    let data = serde_json::to_value(CloseOutcome {
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
    control: &'a Control,
    options: &'a TurnOptions,
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
        ctx.control.check()?;
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
                } else {
                    observed = true;
                }
            }
            Action::Confirm { proposal_id } => {
                if !ctx.options.approvals.contains(proposal_id) {
                    out.refused.push(format!("confirm refused: proposal {proposal_id} has no host-recorded user approval"));
                } else if let Some(note) =
                    confirm_proposal(ctx.store, *proposal_id, ctx.turn, ctx.at)?
                {
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
            Action::OpenGroup { title, acceptance } => {
                let id = open_task(
                    ctx.store,
                    OpenTaskInput {
                        kind: TaskKind::Group,
                        title,
                        request: "",
                        origin: ctx.input,
                        acceptance,
                        turn: ctx.turn,
                        at: ctx.at,
                    },
                )?;
                *focus = Some(id);
                observed = true;
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
                        kind: TaskKind::Node,
                        title,
                        request,
                        origin: ctx.input,
                        acceptance,
                        turn: ctx.turn,
                        at: ctx.at,
                    },
                )?;
                *focus = Some(id);
                observed = true;
                if *run {
                    match reserve_dispatch(ctx, id)? {
                        Ok(assignment) => {
                            pending.push(assignment);
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
                if let Some(note) = link_tasks(ctx.store, *from, *to, *kind, ctx.turn, ctx.at)? {
                    out.refused.push(note);
                }
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
                let (result, error) = match ctx.lookup.look_controlled(
                    *source,
                    target,
                    arguments,
                    selector,
                    &events,
                    ctx.control,
                ) {
                    Ok(value) if serde_json::to_vec(&value)?.len() <= crate::aja::FRAME_LIMIT => {
                        (value, None)
                    }
                    Ok(_) => (
                        Value::Null,
                        Some(
                            "look output exceeds its byte budget; narrow the requested view".into(),
                        ),
                    ),
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
    turn_at_with_options(
        store,
        input,
        brain,
        worker,
        lookup,
        now,
        &TurnOptions::default(),
    )
}

pub fn turn_at_with_options(
    store: &Store,
    input: &str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    lookup: &dyn Lookup,
    now: DateTime<Utc>,
    options: &TurnOptions,
) -> Result<TurnOutcome> {
    let _turn_guard = store
        .active_turn
        .try_lock()
        .map_err(|_| "another turn is already active for this Store")?;
    let _journal_lease = store.lock_turn()?;
    options.limits.validate()?;
    let control = Control::new(
        Duration::from_millis(options.limits.timeout_ms),
        options.cancelled.clone(),
    );
    let at = now.timestamp();
    let turn_no = store.next_turn()?;
    recover(store, turn_no, at)?;
    let existing_tasks = tasks(&store.events()?);
    if options
        .focus
        .is_some_and(|id| !existing_tasks.iter().any(|task| task.id == id))
    {
        return Err("focus names an unknown task".into());
    }
    store.append(
        Kind::User,
        "eja",
        input,
        turn_no,
        EventMeta::data(
            at,
            serde_json::json!({"approved_proposals": options.approvals}),
        ),
    )?;

    let active: Vec<Task> = tasks(&store.events()?)
        .into_iter()
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
        context: project(store, input, focus, now)?,
        tasks: Vec::new(),
        dispatched: Vec::new(),
        refused: Vec::new(),
        evidence: Vec::new(),
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
        lookup,
        control: &control,
        options,
    };
    let mut seen = HashSet::new();
    seen.insert(fingerprint(&out.context));
    let mut refused_this_turn: Vec<String> = Vec::new();
    let mut actions_used = 0usize;
    loop {
        if let Err(error) = control.check() {
            out.end =
                if options.cancelled.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst) {
                    TurnEnd::Cancelled
                } else {
                    TurnEnd::Budget
                };
            out.reason = Some(error.to_string());
            break;
        }
        if out.decisions >= options.limits.decisions {
            out.end = TurnEnd::Budget;
            out.reason = Some("turn decision budget exhausted".into());
            break;
        }
        out.decisions += 1;
        let decision = brain.decide_controlled(&out.context, &control);
        if let Err(error) = control.check() {
            out.end =
                if options.cancelled.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst) {
                    TurnEnd::Cancelled
                } else {
                    TurnEnd::Budget
                };
            out.reason = Some(error.to_string());
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
        let notes = out.refused.len();
        out.refused.extend(decision.refused.iter().cloned());
        let actions_result = apply_actions(&decision.actions, &ctx, &mut out, &mut focus);
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
        let acted = match actions_result {
            Ok(acted) => acted,
            Err(error) if control.check().is_err() => {
                out.end = if options.cancelled.load(Ordering::SeqCst)
                    || INTERRUPTED.load(Ordering::SeqCst)
                {
                    TurnEnd::Cancelled
                } else {
                    TurnEnd::Budget
                };
                out.reason = Some(error.to_string());
                break;
            }
            Err(error) => return Err(error),
        };
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
    recover(store, turn_no, at)?;
    store.append(Kind::Eja, "eja", &out.reply, turn_no, meta)?;
    out.context = project_with_evidence(store, input, focus, now, &out.evidence)?;
    out.tasks = tasks(&store.events()?);
    Ok(out)
}
