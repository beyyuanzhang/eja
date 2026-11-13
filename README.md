# eja — a language-free continuity core

EJA is one persistent subject. It owns **1:n AJA sessions**:

- **EJA**: the one persistent subject
- **AJA**: one disposable session box, one assignment, then closed
- **AJAS**: multiple AJA sessions (plural)

The core is a typed state machine over an append-only log. **It does not read language.**

```
L' = L + e
S  = fold(L)
C  = project(S, input, focus, evidence)
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
| node reader | bounded read of a node's record and the world it left | decide acceptance |
| worker adapter | assignment (objective + acceptance) → AJA run → its record | write identity |
| AJA loop | local transcript, model/tool continuation, last message | access host state or configuration directly |

```
core/       domain, fold, project, turn, ports
aja/        disposable model/tool loop
adapters/   brain, retriever, node reader, worker
app/        CLI and wiring
```

## Core state

- `Event`: `id, at (UTC seconds), turn, scope, kind, text, closes, data`
- `Kind`: user, eja, goal, commitment, correction, proposal,
  proposal_confirmed, proposal_rejected, decision, relationship, task events
  (opened, relation, assigned, worker_report, closed), observed, refused
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

## Look

Every act of reading is one action:

```
look { source: log|node|world, target, selector }
```

- `log` — EJA's own log. A `search` selector asks the retriever for event ids; a
  `task:<id>` or `event:<id>` target reads one record by address.
- `node` — a frozen node record. `whole` reads a bounded file view, `range`
  reads lines, `match` finds a passage with surrounding lines.
- `world` — a read-only connector tool or a project file.

Selectors: `{"select":"whole"}`,
`{"select":"range","from":0,"lines":40}`,
`{"select":"match","text":"PO-1010","before":2,"after":4}`,
`{"select":"search","query":"what did we decide?"}`.

A look creates one `Observed` event and one EVIDENCE entry. Evidence belongs to
the turn that asked for it; the log keeps every look forever. `focus {task_id}`
is navigation, not reading: it chooses the task the turn is about.

A look has no item cap. The model adapter declares how much context it can take,
and the renderer fills the view in priority order; a record that does not fit is
cut where the budget ends and the cut is stated with counts.

## Turn loop

Each decision's actions execute in order through one loop:

```
project → decide → act → append results → project
```

Every look and every AJA dispatch continue through that loop, in any order. A final reply with only state-writing
actions (or no actions) ends the turn. Task focus and recalled evidence remain
available across steps. A turn ends when a decision observes nothing, or when the
projection stops changing: repeated observations that carry no new information are
not spun into an arbitrary cap. Recorded results are always retained, a stalled
turn is logged (`stalled`) and reported by the CLI, and no intermediate reply is
presented as a final answer.

## Judgment

EJA judges worker output itself:

```
node record (what the host saw it do) → EJA decides → close_task { accepted, note }
```

A dispatch returns the host's digest of the node's record — steps, tool calls,
what changed in the project (created, modified, deleted), how it ended, and the
node's last message; the node also records a checkpoint of the world it ended in. A look is optional:
EJA can read the node's whole record, the world it left behind, or the world as
it is now, and decide for
itself. Nothing is required before `close_task`, and there is no separate
verifier. A look at a read-only connector tool lets EJA check the work against
the world when the host grants that view; it never gets an effect tool.

`node.json` keeps the assignment and the digest; the node's whole record — the
start messages, every assistant turn, every tool call with arguments and result,
and its last message — is frozen next to it as `context.jsonl`. The digest is
what the host saw; the last message is the node's own account. Neither is a gate.

## Prompt and strategy

The model prompt carries protocol only: the action vocabulary, the budget and what
a node record means. It does not carry task tactics. What EJA learns about how to
work is recorded in its own identity events — goal, commitment, decision,
correction — and projected back into the next decision. The harness generalizes by
never encoding a per-task hint: if a strategy is worth keeping, EJA writes it down
as identity.

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

The graph is a **projection** of the same event log.
There is no graph database, scheduler, or worker pool. Containment is added
with `relate {kind: part_of}`; `part_of` is containment;
`depends_on`/`continues` are execution order; `relates_to`/`supersedes`/
`contradicts` are association edges.

- one native Rust AJA process per node, started through the same executable;
- the host sends only the node assignment and limits, with an empty environment;
- the node's `request` is its work instruction; `origin` stays in EJA as provenance;
- model access and tools are injected through host IPC; the node has no credentials,
  plugin discovery, MCP client, shell executor, or state-log access;
- one workspace: a node writes the project itself, and its writes are the work;
- the runtime kills/reaps the process, joins its IPC reader and revokes the run
  before committing a checkpoint and atomically publishing `node.json`;
- waiting for EJA's acceptance consumes no AJA process; records remain evidence;
- a node's live transcript compacts against its declared budget; the frozen record
  keeps every tool result;
- the prompt states the node's turn/time budget, and a spent budget ends the
  record with `ended: budget` for a new node instead of a false failure;
- `node.json` also freezes the node's complete context (`context.jsonl`: the start
  messages, every assistant turn, every tool call with arguments and result, and
  the finish entry), read-only. A task that runs through several nodes is audited
  end to end with `eja audit <task>`; one frozen record with `eja node <id>`.

## Recovery

- every append is flushed and `sync_all`'d;
- a torn tail is ignored by the reader;
- on each turn, tasks left `running` by a crash get an interrupted record;
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
cargo run -- --dir /tmp/eja show tasks
cargo run -- --dir /tmp/eja show context
cargo run -- --dir /tmp/eja show log
cargo run -- --dir /tmp/eja show task:1
cargo run -- --dir /tmp/eja show node:1
cargo run -- --dir /tmp/eja show audit:1
```

The current directory is the read-only project input. A node writes only its
project itself. On completion the host commits the workspace to a shadow
repository at `<dir>/checkpoints` and records that commit in the node's manifest;
`look node <id>/<path>` reads a file from that commit.
`<dir>/nodes`. A node id is reserved once and cannot be rerun or overwritten.
Interrupted directories remain reserved; continuation uses a new linked node.

`NativeWorker` runs one node at a time by default; `with_slots`/`EJA_NODE_SLOTS`
declares its capacity, and a decision that dispatches several tasks groups them
by that capacity. Historical nodes are files, not sessions, processes or threads.
`Limits` defaults to 300 seconds, no model-step cap (`EJA_NODE_STEPS` sets one),
256 KiB serialized context/frame, and a 64 KiB model reply. A checkpoint commit is at
most 256 KiB. `with_limits` and `cancel(node_id)` are host APIs; cancellation is
also checked during HTTP requests. These are byte/step limits, not exact model
token limits.

## Tools and connectors

`ToolGateway` is the single host interface. Its per-assignment tool definitions
are snapshotted for a run and checked again on invocation. The gateway receives
the bound node id, work/project roots and cancellation/deadline control. A node
cannot select another id or expand its own grants.

The supplied `FileTools` implements `read_file`, `list_dir`, `write_file` and
`edit_file` (one exact match, so an edit is never ambiguous). Project reads
exclude the private state directory; absolute paths, parent traversal and
symlinks are rejected. Writes land in the workspace and stay there, including partial work
from failed/cancelled nodes. Sealed results cannot be written through this API.
`FileArtifactReader` gives EJA bounded reads of the resulting files; EJA's
observer may also `read_file` and `list_dir` the project, and nothing else.

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

Covers the closed loop, look sources and selectors, proposals/confirmation, recall by
event id, recovery, UTC, projection bounds, worker timeout/cancellation/process cleanup,
the per-node task contract, the native worker IPC and tool loop, bounded/cancelled
HTTP, frozen records, replay protection, resource cleanup, the waiting-task fast
path, and the shared model adapter. MCP integration tests use a local Python 3
fixture (no Python dependencies), covering stdio/HTTP, cancellation, grants,
shared connections and process/session cleanup.

## Not built

- EJA compaction: the log is never summarized; the projection is complete and the
  renderer fits a byte budget the model adapter declares, reporting what it kept.
- tokenizer/pattern matching in core.
- separate verifier authority over EJA.
- parallel workers, multi-writer locks, auth, multi-user.
- MCP startup: servers initialize in order, and the tool catalog is a snapshot
  taken at startup; restart to pick up a changed server.
- typed errors: boundary failures are opaque strings (`Box<dyn Error>`); the CLI
  prints them and adapters keep their context.
- streaming progress/cancel to the user.
