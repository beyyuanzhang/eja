# eja — a language-free continuity core

EJA is one persistent subject. It owns **1:n AJA sessions**:

- **EJA**: the one persistent subject
- **AJA**: one disposable session box, one assignment, then closed
- **AJAS**: multiple AJA sessions (plural)

The core is a typed state machine over an append-only log. **It does not read language.**

```
L' = L + e
S  = fold(L)
C  = project(S, input, focus, recall, artifacts)
d  = Model(C)              // language: adapter
p  = Propose(d)            // identity changes are proposals
S' = commit(S, confirmed actions)
```

## Layers

| Layer | Owns | Must not do |
|---|---|---|
| core | log, fold, projection, typed actions, ids, UTC, recovery | tokenize, match phrases, render prompts |
| brain adapter | language, prompt, proposal JSON | write state directly |
| retriever adapter | semantic query + graph/time expansion → event ids | write state |
| artifact adapter | bounded read of worker output | decide acceptance |
| worker adapter | assignment → AJA session → report | write identity |

```
core/       domain, fold, project, turn, ports
adapters/   brain, retriever, artifact, worker
app/        CLI and wiring
```

## Core state

- `Event`: `id, at (UTC seconds), turn, scope, kind, text, closes, data`
- `Kind`: user, eja, goal, commitment, correction, proposal,
  proposal_confirmed, proposal_rejected, artifact_read, read, task events
- `ArtifactContent`: path, text, truncated, error
- `Proposal`: typed goal/commitment/correction, pending until confirmed

Text is opaque payload. A correction targets an **event id**. Focus is an
explicit `task_id`. Recall returns **event ids**. Artifact reads return
**bounded content**.

The current goal comes only from an active, confirmed `Goal` event. Context
includes that event with its id, or `null` when absent; current input is separate.

## Authority

Identity changes are gated:

```
model proposes
  → proposal event (no identity change)
  → user confirmation
  → confirm(proposal_id)
  → goal/commitment/correction event
```

A proposal cannot be confirmed in the same turn it was created.

- `goal`/`commitment` without `target_id` adds a new entry.
- `goal`/`commitment` with `target_id` replaces an active entry of the same kind.
  One new event carries the replacement text and `closes` the old event.
- `correction` with `target_id` withdraws an active goal or commitment;
  its text records the reason, not a replacement.

Targets are checked at proposal and confirmation time. A stale or mismatched
target cannot change state. Old events remain in the log for reconstruction.

## Recall

`recall(query)` is an action:

```
query → retriever adapter (semantic selection + graph/time expansion)
      → [event ids]
      → core logs a Read event
      → project includes those events
```

No text matching in core.

## Turn loop

Each decision's actions execute in order through one loop:

```
project → decide → act → append results → project
```

Reading tasks, recalling events, reading artifacts and receiving AJA reports all
continue through that loop, in any order. A final reply with only state-writing
actions (or no actions) ends the turn. Task focus and recalled evidence remain
available across steps. One turn permits at most `MAX_TURN_STEPS` (8) decisions.
At the limit, recorded results are retained, `step_limit_reached` is logged and
reported by the CLI, and no intermediate reply is presented as a final answer.
Work can continue on the next turn.

## Artifact reading

EJA reads worker output itself:

```
worker report → artifacts list
EJA: read_artifact(path)
  → artifact adapter reads bounded content under EJA_ARTIFACT_ROOT
  → core logs ArtifactRead
  → EJA context gets the content
  → EJA decides close_task
```

There is no separate verifier that can accept or block on EJA's behalf.
The worker report is a claim; EJA's reading is the evidence.

## Tasks and typed graph

```
open → running → waiting → done | failed
```

An AJA **is** the task point. There is no Task↔AJA 1:n layer:
`EJA : AJA = 1:n`, and `AJA : task = 1:1`. The `Task` record and its
`Assignment` envelope describe the same node; the Assignment is only the
activation message, not a second entity.

A long task is not one Task with many AJAs. It is a connected graph of AJA
nodes. A retry or continuation is a new AJA node linked by `continues` or
`supersedes`, not a second AJA under the same task.

```
child -part_of-> parent
task  -depends_on-> task
aja   -continues-> previous aja
task  -relates_to|supersedes|contradicts-> task
```

The graph is a **projection** of the same event log, bounded by `MAX_RELATIONS`.
There is no graph database, scheduler, or worker pool. Containment is added
with `relate {kind: part_of}`; `part_of` is containment;
`depends_on`/`continues` are execution order; `relates_to`/`supersedes`/
`contradicts` are association edges.

- one AJA session per assignment, timeout `EJA_AJA_TIMEOUT_SECS`;
- `PiWorker` starts Pi directly with the **raw user task**, the configured system
  prompt, cwd and budget; no wrapper prompt;
- the worker gets the **raw user task**, not EJA's acceptance/evidence text;
- the session runs in its own process group; on exit or timeout the whole group
  is terminated, so background children cannot mutate artifacts;
- EJA must be able to read artifacts before accepting;
- no automatic acceptance from a report.

## Recovery

- every append is flushed and `sync_all`'d;
- a torn tail is ignored by the reader;
- on each turn, tasks left `running` by a crash get an interrupted failed report;
- they are not re-dispatched automatically.

## Run

```bash
export LNA_BASE_URL=...
export LNA_API_KEY=...
export LNA_MODEL=...
# direct Pi session box (preferred)
export EJA_PI_MODEL='lna/deepseek-v4.1-flash-1017'
# optional: export EJA_PI_BIN=pi
export EJA_PI_SYSTEM_PROMPT_FILE=/path/to/worker-system-prompt.txt
export EJA_AJA_WORKSPACE=/tmp/eja-work
export EJA_AJA_TIMEOUT_SECS=300

# generic command fallback (assignment JSON on stdin)
# export EJA_AJA_COMMAND='sh examples/aja-template.sh'

export EJA_ARTIFACT_ROOT=/tmp/eja-artifacts

cargo run -- --dir /tmp/eja turn "write a release checklist"
cargo run -- --dir /tmp/eja tasks
cargo run -- --dir /tmp/eja --task 1 context
cargo run -- --dir /tmp/eja log
```

`LNA_RETRIEVER_MODEL` optionally selects a different model for recall.

`PiWorker` reports the files changed during the session as artifacts so EJA can
read and verify them.

## Tests

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Covers the closed loop, artifact reading, proposals/confirmation, recall by
event id, recovery, UTC, projection bounds, worker timeout/process-group cleanup,
the raw-task worker contract, the direct Pi session worker, the waiting-task fast
path, the OpenAI adapter and the command worker.

## Not built

- EJA compaction: the log is never summarized; projection is bounded, not compacted.
- tokenizer/pattern matching in core.
- separate verifier authority over EJA.
- parallel workers, multi-writer locks, auth, multi-user, MCP/skills.
- streaming progress/cancel to the user.
