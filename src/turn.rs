//! The single transition path: validate, record, execute, observe, project.
//!
//! A turn folds the journal once and pushes every event it commits into that
//! same fold, so a decision is validated against one derivation and no action
//! re-reads the log. Nothing here holds a second copy of a fact.
use crate::domain::{ProposalData, TaskOpen};
use crate::state::task_scope;
use crate::{
    now_utc, Action, Assignment, Brain, Choice, ChoiceSelection, CloseOutcome, Control,
    ControlError, Edge, EjaContext, EventMeta, Host, Kind, Observation, ObservationKind,
    ProposalKind, RelationKind, Result, Selector, State, Store, Task, TaskId, TaskKind, TaskStatus,
    TurnEnd, TurnOptions, Worker, WorkspaceGrant,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

/// What one turn did, and the fold it ended with.
///
/// The outcome owns the fold, not a rendered projection: `context` derives the
/// view on demand, so two readers cannot hold two shapes of one fact and no
/// projection outlives the facts it describes.
#[derive(Debug, Clone)]
pub struct TurnOutcome<'a> {
    pub reply: String,
    pub dispatched: Vec<TaskId>,
    pub decisions: usize,
    pub worker_ms: u128,
    pub end: TurnEnd,
    pub reason: Option<String>,
    /// `S`: the logical state the turn ended with.
    state: State,
    /// The input this turn was about, borrowed from the caller like every other
    /// text a view shows.
    input: std::borrow::Cow<'a, str>,
    focus: Option<TaskId>,
    now: chrono::DateTime<chrono::Utc>,
    /// The workspaces this turn projected with, kept so the outcome's view
    /// states the same roots the decisions saw.
    workspaces: Vec<WorkspaceGrant>,
}

impl TurnOutcome<'_> {
    /// `C = view(S, input)`: the projection of the state this turn ended with.
    #[must_use]
    pub fn context(&self) -> EjaContext<'_> {
        self.state
            .project_with_workspaces(&self.input, self.focus, self.now, &self.workspaces)
    }
}

/// What a turn has produced so far. The fold stays separate until the end, so
/// the outcome can own it.
#[derive(Default)]
struct Progress {
    dispatched: Vec<TaskId>,
    decisions: usize,
    worker_ms: u128,
}

/// What one decision produced.
///
/// `committed` follows the state machine's own rule about which actions continue
/// a turn; `informed` is the finer question the fixpoint needs: did they change
/// anything the projection states? A look that repeats the observation it would
/// replace committed a fact and added no information.
#[derive(Debug, Default, Clone, Copy)]
struct Effect {
    committed: bool,
    told: bool,
    informed: bool,
}

impl Effect {
    /// An action whose fact the projection states.
    const fn fact(&mut self) {
        self.committed = true;
        self.informed = true;
    }

    /// A refusal told for the first time this turn.
    const fn noted(&mut self, told: bool) {
        self.told |= told;
        self.informed |= told;
    }

    const fn acted(self) -> bool {
        self.committed || self.told
    }
}

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
fn refuse(store: &Store, state: &mut State, ctx: &ActionCtx<'_>, note: &str) -> Result<bool> {
    if state.refusals().contains(&note) {
        return Ok(false);
    }
    commit(
        store,
        state,
        Kind::Refused,
        "eja",
        note,
        ctx.turn,
        EventMeta::new(ctx.at),
    )?;
    Ok(true)
}

/// Report tasks left Running by a previous process as unknown, never re-run.
///
/// Callers must already own the journal turn (a turn calls it under its lease):
/// this writes facts without taking one itself.
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

/// Record the host's approval of one proposal, as the turn's own fact. The
/// journal stays the authority: a later read sees the approval like any other.
fn approve(
    store: &Store,
    state: &mut State,
    proposal_id: u64,
    turn: u64,
    at: i64,
    source: &str,
) -> Result<()> {
    commit(
        store,
        state,
        Kind::Approval,
        "eja",
        "",
        turn,
        EventMeta::data(
            at,
            serde_json::json!({"proposal_id": proposal_id, "source": source}),
        ),
    )
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
    // A correction withdraws; it asks the user nothing. The host records its
    // approval in the same turn, so the withdrawal can be confirmed without a
    // question ever reaching the user.
    if kind == ProposalKind::Correction {
        approve(store, state, state.last_id(), turn, at, "eja")?;
    }
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
    // A correction withdraws; it never establishes. Against a proposal it is a
    // rejection; against an identity fact it closes the target and keeps the
    // reason as a correction record, not as a new commitment or goal.
    if proposal.kind == ProposalKind::Correction {
        let target = proposal.target_id.expect("correction carries a target");
        let reason = proposal.text.to_string();
        let target_is_proposal = state
            .pending_proposals(turn)
            .iter()
            .any(|pending| pending.id == target);
        if target_is_proposal {
            let note = reject_proposal(store, state, target, &reason, turn, at)?;
            if note.is_none() {
                commit(
                    store,
                    state,
                    Kind::ProposalRejected,
                    "eja",
                    "",
                    turn,
                    EventMeta::data(
                        at,
                        serde_json::json!({"proposal_id": proposal_id, "note": reason}),
                    ),
                )?;
            }
            return Ok(note);
        }
        commit(
            store,
            state,
            Kind::Correction,
            "eja",
            &reason,
            turn,
            EventMeta {
                at,
                closes: Some(target),
                data: Some(serde_json::json!({"proposal_id": proposal_id})),
            },
        )?;
        commit(
            store,
            state,
            Kind::ProposalConfirmed,
            "eja",
            "",
            turn,
            EventMeta::data(at, serde_json::json!({"proposal_id": proposal_id})),
        )?;
        return Ok(None);
    }
    // One event both establishes the replacement and closes its predecessor.
    // The text is borrowed from the fold, which the commit below needs mutably,
    // so it is taken before the commit — the event owns its text anyway.
    let text = proposal.text.to_string();
    commit(
        store,
        state,
        proposal.kind.event_kind(),
        "eja",
        &text,
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

fn open_task(ctx: &ActionCtx<'_>, state: &mut State, input: &TaskOpen<'_>) -> Result<TaskId> {
    let id = state.next_task_id()?;
    commit(
        ctx.store,
        state,
        Kind::TaskOpened,
        &task_scope(id),
        input.title,
        ctx.turn,
        EventMeta::data(ctx.at, serde_json::to_value(input)?),
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
    let graph = state.relations();
    let edge = Edge { from, to, kind };
    if graph.contains(&edge) {
        return Ok(None);
    }
    match kind {
        RelationKind::PartOf => {
            if target.kind != TaskKind::Group || target.status != TaskStatus::Open {
                return Ok(Some(
                    "relate refused: containment needs an open group".into(),
                ));
            }
            if graph
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
                && (target.kind != TaskKind::Aja || target.digest().is_none())
            {
                return Ok(Some(
                    "relate refused: continuation needs a completed aja record".into(),
                ));
            }
            if kind == RelationKind::Continues
                && graph
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
    // A group completes after its members; a aja executes after its explicit
    // dependencies. One combined dependency graph catches cross-kind cycles; the
    // candidate edge is chained in rather than copied into the graph.
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
                    graph
                        .iter()
                        .chain(std::iter::once(&edge))
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
    if !ctx
        .host
        .workspace_available(&task.workspace, &ctx.options.workspaces)
    {
        return Ok(Err(
            "dispatch refused: task workspace is not available".into()
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
            .is_some_and(|other| other.kind == TaskKind::Aja && other.digest().is_some())
    }) {
        return Ok(Err(
            "dispatch refused: continuation needs a completed aja record".into(),
        ));
    }
    let assignment = Assignment {
        task_id: task.id,
        objective: task.request.clone(),
        workspace: task.workspace.clone(),
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
    progress: &mut Progress,
    assignment: &Assignment,
    digest: &str,
) -> Result<()> {
    commit(
        ctx.store,
        state,
        Kind::WorkerReport,
        &task_scope(assignment.task_id),
        digest,
        ctx.turn,
        EventMeta::new(ctx.at),
    )?;
    progress.dispatched.push(assignment.task_id);
    Ok(())
}

/// One execution owner per environment. Reports are recorded in dispatch order, so the
/// journal never depends on thread timing; with more than one slot the environments run
/// at once, and each completed run leaves its evidence before the next report.
fn run_dispatches(
    ctx: &ActionCtx<'_>,
    state: &mut State,
    progress: &mut Progress,
    pending: Vec<Assignment>,
) -> Result<()> {
    let slots = ctx.options.limits.slots.max(1);
    if slots == 1 || pending.len() <= 1 {
        for assignment in pending {
            ctx.control.check()?;
            let task_id = assignment.task_id;
            let started = Instant::now();
            let digest = run_one(ctx.worker, &assignment, ctx.control);
            progress.worker_ms += started.elapsed().as_millis();
            record_dispatch(ctx, state, progress, &assignment, &digest)?;
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
    progress.worker_ms += started.elapsed().as_millis();
    let mut collected = results
        .into_inner()
        .map_err(|_| "dispatch results are unavailable")?;
    collected.sort_by_key(|(index, _, _)| *index);
    for (_, assignment, digest) in collected {
        ctx.control.check()?;
        let task_id = assignment.task_id;
        record_dispatch(ctx, state, progress, &assignment, &digest)?;
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
    // The observation borrows the task's request, so it is serialized before the
    // fold is touched again: one copy of the fact, written once.
    let data = {
        let Some(task) = state.task(task_id) else {
            return Ok(());
        };
        let Some(request) = task.evidence() else {
            return Ok(());
        };
        if task.evidence_id().is_some() {
            return Ok(());
        }
        let target = request["tool"].as_str().unwrap_or("witness");
        let (result, error) = match ctx.host.witness(request, ctx.control) {
            Ok(result) => (result, None),
            Err(error) => (serde_json::Value::Null, Some(error.to_string())),
        };
        let observation = Observation {
            kind: ObservationKind::Witness,
            target,
            arguments: request,
            selector: Selector::Whole,
            result: &result,
            error: error.as_deref(),
        };
        serde_json::to_value(&observation)?
    };
    commit(
        ctx.store,
        state,
        Kind::Observed,
        &task_scope(task_id),
        "",
        ctx.turn,
        EventMeta::data(ctx.at, data),
    )
}

/// Reconciling an environment is the host's act and its account is evidence. Conflicts
/// are recorded, never merged away.
fn reconcile(
    ctx: &ActionCtx<'_>,
    state: &mut State,
    task_id: TaskId,
) -> Result<std::result::Result<(), String>> {
    let Some(task) = state.task(task_id) else {
        return Ok(Err(format!("reconcile refused: no task {task_id}")));
    };
    if task.digest().is_none() {
        return Ok(Err(format!(
            "reconcile refused: task {task_id} has no run to reconcile"
        )));
    }
    if !ctx
        .host
        .workspace_available(&task.workspace, &ctx.options.workspaces)
    {
        return Ok(Err(
            "reconcile refused: task workspace is not available".into()
        ));
    }
    let root = (!task.workspace.is_empty()).then_some(std::path::Path::new(&task.workspace));
    let result = match ctx.host.reconcile(task_id, root, ctx.control) {
        Ok(result) => result,
        Err(error) => {
            let error = error.to_string();
            return Ok(Err(if error.starts_with("reconcile refused:") {
                error
            } else {
                format!("reconcile refused: {error}")
            }));
        }
    };
    let target = task_scope(task_id);
    let arguments = serde_json::json!({"task": task_id});
    let observation = Observation {
        kind: ObservationKind::Reconcile,
        target: &target,
        arguments: &arguments,
        selector: Selector::Whole,
        result: &result,
        error: None,
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
    } else if task.status != TaskStatus::Waiting && (task.status != TaskStatus::Open || accepted) {
        return Ok(Some(format!(
            "close refused: task {task_id} is {}; a aja record is required first",
            task.status.as_str()
        )));
    }
    // Acceptance needs the host's observation, not the runner's word. A
    // rejection needs none: the point of the gate is that EJA may not accept
    // what it cannot verify, not that it must wait forever for a witness that
    // never answers.
    if accepted && task.evidence().is_some() && task.evidence_id().is_none() {
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
    progress: &mut Progress,
    focus: &mut Option<TaskId>,
) -> Result<Effect> {
    let mut effect = Effect::default();
    let mut pending: Vec<Assignment> = Vec::new();
    for action in actions {
        ctx.control.check()?;
        match action {
            Action::RecordDecision { .. } | Action::RecordRelationship { .. } => {
                append_identity(ctx.store, state, action, ctx.turn, ctx.at)?;
            }
            Action::Note { text, closes } => {
                if let Some(target) = closes {
                    let open_note = state.notes().iter().any(|note| note.id == *target);
                    if !open_note {
                        let told = refuse(
                            ctx.store,
                            state,
                            ctx,
                            &format!("note refused: no open note {target}"),
                        )?;
                        effect.noted(told);
                        continue;
                    }
                }
                let meta = closes.map_or_else(
                    || EventMeta::new(ctx.at),
                    |target| EventMeta::closing(ctx.at, target),
                );
                commit(ctx.store, state, Kind::Note, "eja", text, ctx.turn, meta)?;
                effect.fact();
            }
            Action::Propose {
                kind,
                target_id,
                text,
            } => {
                if let Some(note) =
                    propose(ctx.store, state, *kind, *target_id, text, ctx.turn, ctx.at)?
                {
                    let told = refuse(ctx.store, state, ctx, &note)?;
                    effect.noted(told);
                } else {
                    effect.fact();
                }
            }
            Action::Confirm { proposal_id } => {
                if let Some(note) =
                    confirm_proposal(ctx.store, state, *proposal_id, ctx.turn, ctx.at)?
                {
                    let told = refuse(ctx.store, state, ctx, &note)?;
                    effect.noted(told);
                }
            }
            Action::Reject { proposal_id, note } => {
                if let Some(note) =
                    reject_proposal(ctx.store, state, *proposal_id, note, ctx.turn, ctx.at)?
                {
                    let told = refuse(ctx.store, state, ctx, &note)?;
                    effect.noted(told);
                }
            }
            Action::OpenGroup { title, acceptance } => {
                let id = open_task(
                    ctx,
                    state,
                    &TaskOpen {
                        kind: TaskKind::Group,
                        title,
                        request: "",
                        origin: ctx.input,
                        acceptance,
                        workspace: "",
                        evidence: None,
                    },
                )?;
                *focus = Some(id);
                effect.fact();
            }
            Action::OpenTask {
                title,
                request,
                acceptance,
                workspace,
                evidence,
                run,
            } => {
                let workspace = match ctx
                    .host
                    .resolve_workspace(workspace.as_deref(), &ctx.options.workspaces)
                    .map_err(|error| error.to_string())
                {
                    Ok(root) => root,
                    Err(note) => {
                        let told = refuse(ctx.store, state, ctx, &note)?;
                        effect.noted(told);
                        continue;
                    }
                };
                let id = open_task(
                    ctx,
                    state,
                    &TaskOpen {
                        kind: TaskKind::Aja,
                        title,
                        request,
                        origin: ctx.input,
                        acceptance,
                        workspace: &workspace,
                        evidence: evidence.clone(),
                    },
                )?;
                *focus = Some(id);
                effect.fact();
                if *run {
                    match reserve_dispatch(ctx, state, id)? {
                        Ok(assignment) => pending.push(assignment),
                        Err(note) => {
                            let told = refuse(ctx.store, state, ctx, &note)?;
                            effect.noted(told);
                        }
                    }
                }
            }
            Action::Dispatch { task_id } => match reserve_dispatch(ctx, state, *task_id)? {
                Ok(assignment) => {
                    *focus = Some(*task_id);
                    pending.push(assignment);
                    effect.fact();
                }
                Err(note) => {
                    let told = refuse(ctx.store, state, ctx, &note)?;
                    effect.noted(told);
                }
            },
            Action::LinkTasks { from, to, kind } => {
                if let Some(note) =
                    link_tasks(ctx.store, state, *from, *to, *kind, ctx.turn, ctx.at)?
                {
                    let told = refuse(ctx.store, state, ctx, &note)?;
                    effect.noted(told);
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
                    let told = refuse(ctx.store, state, ctx, &reason)?;
                    effect.noted(told);
                }
            }
            Action::Reconcile { task_id } => match reconcile(ctx, state, *task_id)? {
                Ok(()) => effect.fact(),
                Err(note) => {
                    let told = refuse(ctx.store, state, ctx, &note)?;
                    effect.noted(told);
                }
            },
            Action::Focus { task_id } => {
                if !state.knows(*task_id) {
                    let told = refuse(
                        ctx.store,
                        state,
                        ctx,
                        &format!("focus refused: no task {task_id}"),
                    )?;
                    effect.noted(told);
                    continue;
                }
                // Navigating to the task this turn is already about states
                // nothing new, so it is committed but not informative.
                effect.informed |= *focus != Some(*task_id);
                effect.committed = true;
                *focus = Some(*task_id);
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
                let error = error.as_deref();
                let observation = Observation {
                    kind: ObservationKind::Look,
                    target,
                    arguments,
                    selector: selector.clone(),
                    result: &result,
                    error,
                };
                // A read that repeats the observation it would replace states
                // nothing new. The comparison happens before the commit, while
                // both the fold and the new observation can be read.
                effect.informed |= {
                    let evidence = state.evidence();
                    evidence
                        .observations
                        .iter()
                        .find(|previous| previous.same_subject(&observation))
                        .is_none_or(|previous| !previous.same_content(&observation))
                };
                effect.committed = true;
                commit(
                    ctx.store,
                    state,
                    Kind::Observed,
                    "eja",
                    "",
                    ctx.turn,
                    EventMeta::data(ctx.at, serde_json::to_value(&observation)?),
                )?;
            }
        }
    }
    run_dispatches(ctx, state, progress, pending)?;
    Ok(effect)
}

pub fn turn<'i>(
    store: &Store,
    input: &'i str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    host: &dyn Host,
) -> Result<TurnOutcome<'i>> {
    turn_at(store, input, brain, worker, host, now_utc())
}

pub fn turn_at<'i>(
    store: &Store,
    input: &'i str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    host: &dyn Host,
    now: DateTime<Utc>,
) -> Result<TurnOutcome<'i>> {
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

fn stop(end: &mut TurnEnd, reason: &mut Option<String>, error: ControlError) {
    *end = match error {
        ControlError::Cancelled => TurnEnd::Cancelled,
        ControlError::Deadline => TurnEnd::Budget,
    };
    *reason = Some(error.to_string());
}

struct Answer {
    label: String,
    input: String,
    data: Value,
}

/// Resolve a choice against the Eja event that offered it. The stored option is
/// the authority, so a client cannot substitute a different reference.
fn resolve_selection(state: &State, selection: ChoiceSelection) -> Result<Answer> {
    let question = state
        .events()
        .iter()
        .find(|event| event.id == selection.question && event.kind == Kind::Eja)
        .ok_or("selection names no Eja choice question")?;
    let stored = question
        .data
        .as_ref()
        .and_then(|data| data.get("choices"))
        .ok_or("selection names an event with no choices")?;
    let choices: Vec<Choice> = serde_json::from_value(stored.clone())?;
    let choice = choices
        .get(selection.option)
        .ok_or("selection is outside the offered choices")?;
    if choice.label.trim().is_empty() {
        return Err("selected choice has no label".into());
    }
    let already_answered = state.events().iter().any(|event| {
        event.kind == Kind::User
            && event
                .data
                .as_ref()
                .and_then(|data| data.get("choice"))
                .and_then(|choice| choice.get("question"))
                .and_then(Value::as_u64)
                == Some(selection.question)
    });
    if already_answered {
        return Err("choice question was already answered".into());
    }
    Ok(Answer {
        label: choice.label.trim().to_string(),
        input: format!(
            "{}\nreference: {}",
            choice.label.trim(),
            serde_json::to_string(&choice.value)?
        ),
        data: serde_json::json!({
            "choice": {
                "question": selection.question,
                "option": selection.option,
                "value": choice.value,
            }
        }),
    })
}

pub fn turn_at_with_options<'i>(
    store: &Store,
    input: &'i str,
    brain: &dyn Brain,
    worker: &dyn Worker,
    host: &dyn Host,
    now: DateTime<Utc>,
    options: &TurnOptions,
) -> Result<TurnOutcome<'i>> {
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
    let situation = match options.gesture {
        None => None,
        Some(kind @ (Kind::Returned | Kind::Departed | Kind::Alone)) => Some(kind),
        Some(_) => return Err("gesture names an unknown situation".into()),
    };
    if situation.is_some() && !options.approvals.is_empty() {
        return Err("a situation carries no approvals".into());
    }
    if situation.is_some() && !options.denials.is_empty() {
        return Err("a situation carries no denials".into());
    }
    if situation.is_some() && options.focus.is_some() {
        return Err("a situation carries no focus".into());
    }
    let answer = options
        .selection
        .map(|selection| resolve_selection(&state, selection))
        .transpose()?;
    if answer.is_some()
        && (situation.is_some()
            || !input.trim().is_empty()
            || !options.approvals.is_empty()
            || !options.denials.is_empty())
    {
        return Err("a choice answer is its own turn input".into());
    }
    if input.trim().is_empty()
        && answer.is_none()
        && options.approvals.is_empty()
        && options.denials.is_empty()
        && situation.is_none()
    {
        return Err("a turn needs text, a choice, an approval, a denial or a situation".into());
    }
    if situation.is_none() {
        if let Some(answer) = &answer {
            commit(
                store,
                &mut state,
                Kind::User,
                "eja",
                &answer.label,
                turn_no,
                EventMeta::data(at, answer.data.clone()),
            )?;
        } else if !input.trim().is_empty() {
            commit(
                store,
                &mut state,
                Kind::User,
                "eja",
                input,
                turn_no,
                EventMeta::new(at),
            )?;
        }
    }
    if let Some(kind) = situation {
        commit(
            store,
            &mut state,
            kind,
            "eja",
            "",
            turn_no,
            EventMeta::new(at),
        )?;
    }
    let input: std::borrow::Cow<'i, str> = if situation.is_some() {
        match situation {
            Some(Kind::Returned) => "user returned".into(),
            Some(Kind::Departed) => "user departed".into(),
            Some(Kind::Alone) => "alone; time passes".into(),
            _ => "".into(),
        }
    } else if let Some(answer) = &answer {
        answer.input.clone().into()
    } else if input.trim().is_empty()
        && (!options.approvals.is_empty() || !options.denials.is_empty())
    {
        // The user's answer is this turn's input: a fact, not invented words.
        let mut parts = Vec::new();
        if !options.approvals.is_empty() {
            parts.push(format!(
                "user approved proposal(s) {}",
                options
                    .approvals
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !options.denials.is_empty() {
            parts.push(format!(
                "user denied proposal(s) {}",
                options
                    .denials
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.join("; ").into()
    } else {
        input.into()
    };
    // An approval is a fact, not a caller's flag: it is recorded once, with the
    // turn it authorizes, and every later read comes from the journal.
    for proposal_id in &options.approvals {
        approve(store, &mut state, *proposal_id, turn_no, at, "user")?;
    }
    // A denial is the user's answer, recorded like an approval: the proposal
    // stays pending until EJA itself withdraws or re-asks.
    for proposal_id in &options.denials {
        commit(
            store,
            &mut state,
            Kind::Denial,
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
        if situation.is_none() && active.len() == 1 && active[0].status == TaskStatus::Waiting {
            Some(active[0].id)
        } else {
            None
        }
    });
    let ctx = ActionCtx {
        store,
        input: &input,
        turn: turn_no,
        at,
        worker,
        host,
        control: &control,
        options,
    };
    let mut progress = Progress::default();
    let mut reply = String::new();
    let mut choices = Vec::new();
    let mut end = TurnEnd::Complete;
    let mut reason: Option<String> = None;
    let mut actions_used = 0usize;
    loop {
        if let Err(error) = control.check() {
            stop(&mut end, &mut reason, error);
            break;
        }
        if progress.decisions >= options.limits.decisions {
            end = TurnEnd::Budget;
            reason = Some("turn decision budget exhausted".into());
            break;
        }
        progress.decisions += 1;
        // The projection is a borrow of the fold, so it lives exactly as long as
        // this decision: the actions that follow commit facts, which needs the
        // fold mutably.
        let decision = {
            let context = state.project_with_workspaces(&input, focus, now, &options.workspaces);
            brain.decide(&context, &control)
        };
        if let Err(error) = control.check() {
            stop(&mut end, &mut reason, error);
            break;
        }
        if decision.actions.len() > options.limits.actions.saturating_sub(actions_used) {
            end = TurnEnd::Budget;
            reason = Some("turn action budget exhausted".into());
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
                .saturating_sub(progress.dispatched.len())
        {
            end = TurnEnd::Budget;
            reason = Some("turn dispatch budget exhausted".into());
            break;
        }
        actions_used += decision.actions.len();
        for note in &decision.refused {
            refuse(store, &mut state, &ctx, note)?;
        }
        let effect = match apply_actions(
            &decision.actions,
            &ctx,
            &mut state,
            &mut progress,
            &mut focus,
        ) {
            Ok(effect) => effect,
            Err(error) => {
                if let Err(stopped) = control.check() {
                    stop(&mut end, &mut reason, stopped);
                    break;
                }
                return Err(error);
            }
        };
        // A decision that committed nothing is a final reply. One that committed
        // facts but states nothing new is the turn's fixpoint: the kernel knows
        // that from what each action produced, without keeping a second copy of
        // the projection to compare against.
        if !effect.acted() {
            reply = decision.reply.trim().to_string();
            choices = decision.choices;
            break;
        }
        if !effect.informed {
            end = TurnEnd::Stalled;
            break;
        }
    }
    let mut meta = EventMeta::new(at);
    if end != TurnEnd::Complete {
        reply = format!(
            "[turn {:?}] {}",
            end,
            reason.as_deref().unwrap_or("projection repeated")
        );
        choices.clear();
        meta.data = Some(serde_json::json!({"end": end, "reason": reason}));
    }
    recover_state(store, &mut state, turn_no, at)?;
    // A turn that asks the user a question delivers the question, not a reply:
    // the answer comes first, and EJA speaks to it in the turn the answer opens.
    // The model's text for this turn stays its monologue. A correction the host
    // approved itself asks nothing, so it does not silence the turn.
    let fresh_question = state
        .pending_proposals(turn_no)
        .iter()
        .any(|proposal| proposal.turn == turn_no && !proposal.approved);
    if fresh_question && end == TurnEnd::Complete {
        reply = String::new();
        choices.clear();
    }
    if !choices.is_empty() {
        meta.data = Some(serde_json::json!({"choices": choices}));
    }
    commit(store, &mut state, Kind::Eja, "eja", &reply, turn_no, meta)?;
    Ok(TurnOutcome {
        reply,
        dispatched: progress.dispatched,
        decisions: progress.decisions,
        worker_ms: progress.worker_ms,
        end,
        reason,
        state,
        input,
        focus,
        now,
        workspaces: options.workspaces.clone(),
    })
}
