# eja — a persistent subject with disposable work nodes

EJA owns durable identity, task structure and acceptance decisions. An AJA is one
bounded execution node. AJAS is the plural. A group collects nodes; it never
starts a process. The core treats language as opaque data.

```
S = fold(L)
C = view_B(S, input, evidence)
A = model(C)
(O, W') = execute(A, W, grants, control)
L' = L ++ events(A, O)
```

The [architecture contract](docs/ARCHITECTURE.md) defines the invariants and the
composition laws. The implementation has five core modules: domain, store,
state, turn, and the shared control. Adapters own model transport, connectors and
record reading.

## Facts and views

- `log.jsonl` is the authority for identity, task lifecycles, graph edges,
  observations and judgments. Task tables and the graph are projections.
- `<dir>/nodes/<id>/context.jsonl` records that node's own start messages,
  assistant messages and full tool results. A continuation stores its parent
  address, rather than copying the parent's payload into its record.
- `node.json` freezes the assignment, digest and optional checkpoint address.
  The digest is an index into evidence, not a verdict.
- Working contexts are bounded views. Live tool results and replayed tool results
  use the same view function. Every tool result has a `{node, entry}` address;
  `eja_read_record` reads a bounded passage from this node or its ancestry.
- A checkpoint captures Git-visible workspace files. It is evidence about the
  world at that time; a continuation inherits observations, not a restored world.
  Missing checkpoints are reported in both manifest and digest.

## Tasks and authority

```
node:  open -> running -> waiting -> done | failed
       open -> failed                 (cancel before dispatch)
group: open -> done | failed           (after children settle)
```

Use `open_task` for executable work, and `open_group` for a container. A group
cannot dispatch. Closing a group requires its children to have settled. EJA judges the group
against its acceptance contract; a failed child does not automatically determine
the group verdict. Empty groups need no dummy execution. Containment has
one parent per task. Dependency cycles include both execution dependencies and
the fact that a group completes after its children.

`continues` has one frozen predecessor. `depends_on` requires an accepted
predecessor before dispatch. Both are fixed before dispatch. Relations are
validated before entering the log. `relates_to`, `supersedes` and `contradicts`
are descriptive edges; they do not silently change task state.

Goals, commitments and corrections start as proposals. Confirmation requires an
explicit approval id supplied by the host in `TurnOptions.approvals`, recorded
with that turn's user event, and a proposal from an earlier turn. Model output
alone cannot authorize confirmation. One identity event applies the change and
resolves the proposal atomically. Existing confirmation events remain readable.

EJA may observe any host-granted read-only capability. Its ability to observe
never depends on unrelated tasks. Effects occur through an AJA's host gateway.
MCP read-only declarations are scoped to their server and tool.

## Reading and continuity

```
look {source: log | node | world, target, selector}
```

- Log targets: `task:<id>`, `event:<id>`, `history`, `all`. Discovery uses event
  indices with `range`; whole returns the latest 40 matching events. Pages expose
  `from`, `next`, `total` and `truncated`. Oversized events expose their id in
  `partial_events`; `event:<id>` supports byte views so paging always advances.
- Node targets: `<id>/context.jsonl`, `<id>/node.json`, `<id>/<workspace-file>`.
  Whole, range and match use the same source resolver, including checkpoints.
- World targets: `read_file`, `list_dir`, or a granted read-only connector.
  World tools select content through their own arguments.

Selectors are `{"select":"whole"}`, `{"select":"range","from":0,"lines":40}`,
`{"select":"match","text":"passage","before":2,"after":4}`, and
`{"select":"bytes","from":0,"bytes":4096}` for a serialized log event.
History is projected newest first; omitted history remains discoverable by id or
page. Views report truncation and count records. The rendered brain context
never exceeds its declared UTF-8 byte budget, including notices and headers.

## Fold

One renderer produces every view the model sees, for EJA and for a running node
alike. A view is a space of regions in priority order — the newest step first,
the current input first, evidence before the index — and the renderer chooses a
scale for each region inside the declared budget:

| Scale | What the model gets |
|---|---|
| record | the region whole |
| index | one line, plus the handle that reaches it |
| map | the region's shortest line, or one line for a folded range of them |

The head stays whole while the budget allows, the rest is listed, and the oldest
folds into an address. Nothing is summarized by a model and nothing is deleted:
every folded region keeps its handle in the log or in the node record, so
reading it again is an address lookup. A node whose transcript no longer fits
even at map scale **stops** (`ended: budget`) and continues in a new node; it
never keeps re-sending a transcript pinned at the cap.

## Execution and resources

A turn runs `project -> decide -> act -> record` until a final decision, repeated
projection, cancellation, or a host budget. `TurnEnd` distinguishes these cases.
Creating a node, group or proposal returns its allocated id through the next
projection, so the model never needs to guess a new id. Budget/cancellation results are explicit; intermediate replies are never final.
Every reserved running task is reconciled when execution stops.

Default host budgets:

| Resource | Default |
|---|---:|
| Turn decisions / actions / dispatches | 128 / 4096 / 128 |
| Turn elapsed time | 30 minutes |
| Node model steps / elapsed time | 256 / 300 seconds |
| Node context / model response | 252 KiB / 64 KiB |
| IPC JSON payload | 256 KiB |
| One tool-result working view | 8 KiB, including address |
| Record passage read | at most 1 KiB of UTF-8 text |
| Brain rendered context | 200,000 bytes |

One `Control` hierarchy propagates the turn deadline/cancellation to the brain,
nodes, observations and model/tool calls. A child may shorten the deadline, never
extend it. Every port requires this control explicitly; there is one entry point
per operation. Dispatches run in action order under one execution owner, and each
completed report is recorded before the next node starts. The host owns node step
and time limits; the disposable loop only manages its bounded message view.

## Recovery

A newline commits an event. Appends are serialized, flushed and synced. An
uncommitted final fragment is discarded under the writer lock before more
appends; malformed committed records are errors. Stale Store instances refuse
to append. The journal allows only one active turn, including across separate Store instances. These guards fail closed; they
are not a distributed multi-writer system.

Interrupted nodes are never dispatched again under the same id. Their task
becomes waiting for judgment, and continuation uses a new id. Frozen records
retain the complete safe prefix of an interrupted tool-call group. Damaged or
missing predecessor records are explicit errors, never a silent fresh start.

## Run

```bash
export LNA_BASE_URL=https://your-provider/v1
export LNA_API_KEY=...
export LNA_MODEL=...
# Optional node model: EJA_AJA_MODEL

cargo run -- --dir /tmp/eja turn "draft a release checklist"
cargo run -- --dir /tmp/eja show tasks
cargo run -- --dir /tmp/eja show task:1
cargo run -- --dir /tmp/eja show node:1
cargo run -- --dir /tmp/eja show audit:1
cargo run -- --dir /tmp/eja show context
cargo run -- --dir /tmp/eja --confirm 12 turn "apply the approved proposal"
```

Configure connectors in `<dir>/mcp.json`, or pass `--mcp-config FILE`.
`eja --dir DIR show tools` lists connector grants without model credentials.
Without connectors, a node has its record reader and can produce a reply; it
has no filesystem-write or external-effect capability. See [MCP setup](docs/MCP.md)
and the [native protocol](docs/AJA-PROTOCOL.md).

Host resource configuration: `EJA_TURN_DECISIONS`, `EJA_TURN_ACTIONS`,
`EJA_TURN_DISPATCHES`, `EJA_TURN_TIMEOUT_MS`, `EJA_NODE_STEPS`,
`EJA_NODE_TIMEOUT_MS`, `EJA_CONTEXT_BYTES`. Limits must be positive.
`EJA_NODE_SLOTS` must be absent or `1`: one mutable workspace has one execution
owner, so any other value is refused instead of starting a second runner.
`--task ID` selects initial focus. `show audit:ID` follows explicit containment
and continuation edges; matching input text never invents a job relationship.

## Validation and limits

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Composition regressions cover unrelated-task invariance, large-result replay,
record addressability, aggregate completion, relation uniqueness and cross-kind
cycles, host authorization, atomic confirmation, torn-tail recovery, UTF-8 byte
budgets, deadlines and always-changing model states. Native/MCP tests also cover
real local HTTP, cancellation, process cleanup and frozen checkpoints.

Trusted adapters must honor Control, own their resources and bound their output.
An ordinary child process is not an OS security sandbox. Cancellation cannot
undo remote side effects. The kernel guarantees bounded host execution under
these port contracts; it cannot prove a model will solve an arbitrary objective.
Identity and task state can be replayed; world effects are never replayed by
folding the log. Legacy records/logs remain readable; past invalid relationships
are not retroactively authorized, and dispatch refuses ambiguous ancestry.

Historical, pre-0.11 model measurements are kept separately in
[benchmark history](docs/BENCHMARKS.md); they are not measurements of this revision.
