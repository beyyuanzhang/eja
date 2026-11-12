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
7. A report is a claim. EJA decides; nothing else can accept on its behalf.
   Reading artifacts or the outside world is an option, never a required gate.
8. A task closes only from `waiting` with `close_task`.
9. AJA : task = 1:1. The native worker uses one temporary Rust process per
   assignment and one execution slot. Finishing a node freezes its complete
   context: the manifest, the assignment, the start messages, every assistant
   turn, every tool call with arguments and result, and the report, all read-only.
   No session survives; retries/continuations are new linked nodes.
10. Language, prompts, retrieval and artifact reading live in adapters.
11. A crashed `running` task is reconciled to `waiting` with an interrupted
    failed report; no automatic duplicate dispatch.
12. The worker receives its node's `request`; the original input stays in EJA
    as `origin`. Acceptance remains EJA's responsibility.
13. If exactly one active task is waiting, the turn starts with that task as
    focus, avoiding one redundant initial model call.
14. All actions execute in list order. Observations continue through one loop that
    ends at a fixpoint: a decision that observes nothing, or a projection that
    repeats. Recorded results are always retained and a stalled turn is reported
    explicitly; pure state-writing actions can finish.
15. Prompts carry protocol only: actions, limits and what a report means. Task
    strategy is not written in code or prompt text; it is what EJA records in its
    own identity events (goal, commitment, decision, correction).

## Core API

- `Store`: append/read events; flush + `sync_all`; tolerant torn tail.
- `current_goal`, `open_commitments`, `active_identity`, `tasks`, `relations`.
- `project`: structured context (goal, identity, commitments, task index,
  focus, recall, artifact reads, pending proposals, now).
- `turn_at`: one project/decide/act loop for typed actions and observations; it
  ends at a fixpoint and reports `decisions`, `worker_ms` and `stalled`.
- `aja`: bounded disposable loop and framed IPC, no external effects.
- `NativeWorker`: host model/tool gateway, node workspace, cancellation, process
  cleanup, artifact sealing and frozen manifest. One run per node id.
- `ToolGateway`: per-node tool definitions and host execution, with scoped paths
  and deadline/cancellation control. `HostTools` combines file tools and host-owned
  `McpTools` (stdio / Streamable HTTP). Nodes share connections, not configuration.
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
| `observe {tool, arguments}` | read-only connector tool → `Observed` event and OBSERVATIONS view |

## Modules

```
core/       src/lib.rs
aja/        src/aja.rs (temporary model/tool loop)
adapters/   brain, retriever, model, artifact, tools, mcp, native
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
- OS security sandboxing, progress streaming and UI cancel.
  NativeWorker exposes a host cancellation API.
