# Architecture

## Formula

```
L' = L + e
S  = fold(L)
C  = project(S, input, focus, recall, artifacts)
d  = Model(C)
p  = Propose(d)
a  = ArtifactReader(path)   // EJA-initiated, read-only
S' = commit(S, confirmed actions)
```

## Invariants

1. `log.jsonl` is the only truth; append-only, never compacted.
2. Text is opaque payload. Core code never tokenizes, matches, or interprets it.
3. State transitions accept typed actions with ids/refs/timestamps.
4. A correction withdraws an event by id. A goal/commitment with `closes`
   replaces an active event of the same kind; no phrase matching.
5. Focus is an explicit task id; recall returns event ids from an adapter.
6. Identity changes require a proposal and a later confirmation.
7. A report is a claim. EJA reads artifacts and decides; no external verifier
   can accept on EJA's behalf.
8. A task closes only from `waiting` with `close_task`.
9. One AJA session runs at a time, in its own process group, bounded by a
   timeout; the whole group is terminated on exit or timeout. EJA may own many
   AJA sessions over time (AJAS); EJA : AJA = 1:n. An AJA is the task point:
   AJA : task = 1:1. A retry/continuation is a new AJA node, not a second AJA
   under the same task.
10. Language, prompts, retrieval and artifact reading live in adapters.
11. A crashed `running` task is reconciled to `waiting` with an interrupted
    failed report; no automatic duplicate dispatch.
12. The worker receives the raw user task (`origin`), not EJA's acceptance or
    evidence text.
13. If exactly one active task is waiting, the turn starts with that task as
    focus, avoiding one redundant initial model call.
14. All actions execute in list order. Observations continue through one loop,
    bounded by `MAX_TURN_STEPS`. Exhaustion retains recorded results for a later
    turn and is reported explicitly; pure state-writing actions can finish.

## Core API

- `Store`: append/read events; flush + `sync_all`; tolerant torn tail.
- `current_goal`, `open_commitments`, `active_identity`, `tasks`, `relations`.
- `project`: structured context (goal, identity, commitments, task index,
  focus, recall, artifact reads, pending proposals, now).
- `turn_at`: one bounded project/decide/act loop for typed actions and observations;
  reports `decisions`, `worker_ms` and `step_limit_reached`.
- `PiWorker`: direct session box. It does not re-author the task or pass EJA
  acceptance into the session. It reports files changed during the session as
  artifacts.
- `recover`: reconcile interrupted assignments.
- Ports: `Brain`, `Worker`, `Retriever`, `ArtifactReader`.

## Actions

| Action | Meaning |
|---|---|
| `propose {kind, target_id?, text}` | add a goal/commitment; with a target, replace the same kind or withdraw via correction |
| `confirm {proposal_id}` | apply a proposal created in an earlier turn |
| `reject {proposal_id, note?}` | resolve a proposal without changing identity |
| `decision/relationship {text}` | append a non-authorizing identity note |
| `relate {from, to, kind}` | add a typed task/AJA edge |
| `open_task {…, run}` | create a task; optionally dispatch |
| `dispatch {task_id}` | run an open task |
| `close_task {task_id, accepted, note}` | verdict on a waiting task |
| `read_task {task_id}` | structured task focus |
| `recall {query}` | retriever adapter → event ids |
| `read_artifact {path}` | artifact adapter → bounded content in EJA context |

## Modules

```
core/       src/lib.rs
adapters/   brain, retriever, artifact, worker
app/        src/main.rs
```

Core does not import adapters. Adapters implement ports; they never mutate
state directly.

## Time

UTC seconds from one clock per turn. Task timestamps, assignment `issued_at`
and recovery events use the same `at`. The adapter renders times; core stores
numbers.

## Non-goals (v1)

- compaction / summarizing the log;
- keyword authorization;
- lexical recall in core;
- external verifier authority;
- parallel workers, retries, multi-user, auth;
- worker sandboxing, progress streaming, user-visible cancel.
