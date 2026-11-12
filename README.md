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
| AJA loop | local transcript, model/tool continuation, final report | access host state or configuration directly |

```
core/       domain, fold, project, turn, ports
aja/        disposable model/tool loop
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
available across steps. A turn ends when a decision observes nothing, or when the
projection stops changing: repeated observations that carry no new information are
not spun into an arbitrary cap. Recorded results are always retained, a stalled
turn is logged (`stalled`) and reported by the CLI, and no intermediate reply is
presented as a final answer.

## Judgment

EJA judges worker output itself:

```
worker report (a claim) → EJA decides → close_task { accepted, note }
```

`read_artifact` is an optional observation. When a node wrote files, EJA can read
them; when the work lives in an external system (a connector or a service), the
artifact list is legitimately empty and EJA decides from the report. Nothing is
required before `close_task`, and there is no separate verifier.

`node.json` keeps the host-observed tool log of the node in the sealed report
(`report.tools`), and the current task renders it. It is a record, not a gate.

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

- one native Rust AJA process per node, started through the same executable;
- the host sends only the node assignment and limits, with an empty environment;
- the node's `request` is its work instruction; `origin` stays in EJA as provenance;
- model access and tools are injected through host IPC; the node has no credentials,
  plugin discovery, MCP client, shell executor, or state-log access;
- node work areas are independent; project files are read-only through host tools;
- the runtime kills/reaps the process, joins its IPC reader and revokes the run
  before sealing artifacts and atomically publishing `node.json`;
- waiting for EJA's acceptance consumes no AJA process; reports remain claims;
- the prompt states the node's turn/time budget, and a spent budget returns a
  `blocked` report for a new node instead of a false failure;
- `node.json` also freezes a bounded host-observed tool log, for audit only.

## Recovery

- every append is flushed and `sync_all`'d;
- a torn tail is ignored by the reader;
- on each turn, tasks left `running` by a crash get an interrupted failed report;
- they are not re-dispatched automatically.

## Run

```bash
export LNA_BASE_URL=https://your-provider/v1
export LNA_API_KEY=...
export LNA_MODEL=...
# Optional separate model for work nodes and recall:
# export EJA_AJA_MODEL=...
# export LNA_RETRIEVER_MODEL=...

cargo run -- --dir /tmp/eja turn "write a release checklist"
cargo run -- --dir /tmp/eja tasks
cargo run -- --dir /tmp/eja --task 1 context
cargo run -- --dir /tmp/eja log
```

The current directory is the read-only project input. A node writes only its
private `<dir>/nodes/<id>/work` area. On completion, work is removed and artifacts
are copied into `<dir>/nodes/<id>/artifacts`; returned references are relative to
`<dir>/nodes`. A node id is reserved once and cannot be rerun or overwritten.
Interrupted directories remain reserved; continuation uses a new linked node.

`NativeWorker` has one execution slot. Historical nodes are files, not sessions,
processes or threads. `Limits` defaults to 300 seconds, 16 model steps, 256 KiB
serialized context/frame, and 64 KiB model/tool output. A sealed artifact is at
most 256 KiB. `with_limits` and `cancel(node_id)` are host APIs; cancellation is
also checked during HTTP requests. These are byte/step limits, not exact model
token limits.

## Tools and connectors

`ToolGateway` is the single host interface. Its per-assignment tool definitions
are snapshotted for a run and checked again on invocation. The gateway receives
the bound node id, work/project roots and cancellation/deadline control. A node
cannot select another id or expand its own grants.

The supplied `FileTools` implements `read_file` and `write_file`. Project reads
exclude the private state directory; absolute paths, parent traversal and
symlinks are rejected. Writes are recorded as artifacts, including partial work
from failed/cancelled nodes. Sealed results cannot be written through this API.
`FileArtifactReader` gives EJA bounded reads of the resulting files.

`HostTools` combines file tools with optional MCP tools. The host owns the MCP
configuration, credentials, runtime and connections; all nodes reuse them through
the same gateway. Both stdio and Streamable HTTP (JSON/SSE) are supported by the
official Rust SDK. Put `mcp.json` in the chosen `--dir`, or pass
`--mcp-config FILE`. `eja --dir DIR tools` connects, prints the granted tool catalog
and closes the connections without needing model credentials. See [MCP setup](docs/MCP.md).

Trusted adapters must honor `Control`, bound outputs, and own/settle any resources
or operations they create. Ordinary same-user child processes and read-only file
permissions are not an operating-system security sandbox. Native AJA itself only
performs IPC and has no tool-created descendants; its pipe closes when the host
exits. External side effects require the connector's own cancellation semantics.

## Tests

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Covers the closed loop, artifact reading, proposals/confirmation, recall by
event id, recovery, UTC, projection bounds, worker timeout/cancellation/process cleanup,
the per-node task contract, the native worker IPC and tool loop, bounded/cancelled
HTTP, frozen artifacts, replay protection, resource cleanup, the waiting-task fast
path, and the shared model adapter. MCP integration tests use a local Python 3
fixture (no Python dependencies), covering stdio/HTTP, cancellation, grants,
shared connections and process/session cleanup.

## Not built

- EJA compaction: the log is never summarized; projection is bounded, not compacted.
- tokenizer/pattern matching in core.
- separate verifier authority over EJA.
- parallel workers, multi-writer locks, auth, multi-user.
- streaming progress/cancel to the user.
