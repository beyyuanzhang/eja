# eja — one persistent subject, disposable executions

EJA owns durable identity, task structure and acceptance decisions. An **AJA** is
one bounded execution of it: a disposable process with its own transcript, tools
and record. AJAS is the plural. A group gathers ajas; it never starts a process.
The core treats language as opaque data.

```
S = fold(L)
C = view_B(S, input)
A = model(C)
(O, W') = execute(A, W, grants, control)
L' = L ++ events(A, O)
```

The [architecture contract](docs/ARCHITECTURE.md) defines the invariants and the
composition laws. The implementation has six core modules: domain, store, state,
turn, the shared control, and the one renderer `fold`. Adapters own model
transport, connectors and record reading: one record module owns the manifest, the
path resolver, the reader and the shadow repository that freezes a run.

A task that executes carries its run facts together (`Run`): its account and the
observation that answers its acceptance contract. A group aggregates and never
runs, so it has none of them and no reader has to ask whether a field means
anything for the kind it is looking at.

`S = fold(L)` is one value: `State` applies the journal event by event, and a
turn reads the journal once and pushes every event it commits into the same
value. Nothing validates an action against a second derivation of the log.

`C = view(S, input)` is a borrowed view: what a decision reads holds references
into `S` and the input, so a projection copies no fact. A turn's outcome owns
`S` and derives the view on demand; no projection outlives the facts it shows.

## The prototype and its constitution

The formula above is the whole prototype. Every mechanism must name which part
of the loop it serves; nothing gets a layer of its own:

- **Sensation** — facts entering `L` (your words, approvals, situations,
  observations). All writes go through the one turn entry; the host only
  offers facts, never meaning.
- **Memory** — `fold`, plus what EJA itself keeps: an open `note` is a memory,
  closing it is letting go. Notes are inner records, not speech.
- **Attention** — the projection and its slot order; what is salient is what
  the view shows, and EJA shapes it by what it writes and keeps.
- **Judgment** — `model(C)`, the only place meaning is generated.
  A reply is EJA's voice: it reaches the user in a turn they spoke in or
  returned in, and stays inner monologue in a turn they are absent from.
  Whether to speak at all is EJA's decision; silence leaves the return screen
  quiet.
- **Action** — `execute`, whose observations flow back into `L`, closing the loop.
- **Rhythm** — the host's single `Pulse`: how often it looks, and how much quiet
  opens an `alone` situation. It is policy, not a second source of time.

Constitution:

1. One authority per function. `log.jsonl` is the only source of truth; the API
   serializes the one core fold and never re-derives it.
2. Time has one source: the OS clock, sampled once per turn and stamped on every
   event of that turn. The core keeps no clock; the host compares journal stamps.
3. Whatever EJA is, it must point at an event it wrote or witnessed. No persona
   or fixed vocabulary is injected; the prompt states mechanics only.
4. The subject persists; executions are disposable.
5. Silence and inaction are whole outcomes at every step.
6. Every action's observation returns to the journal as a fact.

## App

`app/` is an independently served Svelte 5 + TypeScript + Vite interface for
continuous conversation with EJA. It has one attention surface and two views over
the same journal: **talk** is either the return screen of open loops or one
active encounter, and **unfold** shows the task lanes, timing, dependencies and
evidence. There is no new-chat or session control.

The HTTP runtime is part of the `eja` executable; the frontend is not embedded:

```sh
cargo run -- serve --dir .eja --project .
npm --prefix app/frontend run build
npm --prefix app/frontend run preview
```

Open <http://127.0.0.1:4173>. That is the only app address. The runtime's
<http://127.0.0.1:8787> endpoint is internal API transport, not a second app.
Any number of separately hosted frontends may use the same runtime. See
[app/README.md](app/README.md).

## Facts and views

- `log.jsonl` is the authority for identity, task lifecycles, graph edges,
  observations and judgments. Task tables and the graph are projections.
- `<dir>/ajas/<id>/context.jsonl` records that aja's own start messages,
  assistant messages and full tool results. A continuation stores its parent
  address, rather than copying the parent's payload into its record.
- `aja.json` is one typed manifest: the assignment, the digest, and the optional
  checkpoint address a `record:` file is read from. The digest is an index into
  evidence, not a verdict.
- Working contexts are bounded views. Live tool results and replayed tool results
  use the same view function. Every tool result has a `{aja, entry}` address;
  `eja_read_record` reads a bounded passage from this aja or its ancestry.
- A checkpoint captures Git-visible workspace files. It is evidence about the
  world at that time; a continuation inherits observations, not a restored world.
  Missing checkpoints are reported in both manifest and digest. The record store
  owns that repository and its one hermetic git invocation, so a project's hooks,
  signing settings and global configuration never reach a run.
- An approval is its own event, recorded by the host for one turn. Confirmation
  reads that event, so a caller's flag never becomes a second authority.
- A task may declare **evidence** at open: what the host must observe before the
  task can be judged. The host's observation is recorded as one `Observed` event
  scoped to the task. Acceptance needs that observation; a rejection does not,
  so a witness that never answers cannot strand a task in the index.
- One address space covers every fact a view can read: `log:<target>`,
  `record:<aja>/<path>`, `world:<path>`. Any other target is a host read
  tool. A folded region and a live one are read the same way.
- An observation says what it is — `look`, `witness` or `reconcile` — and its
  target then names the subject. Its kind is a typed field, not a reserved text,
  so a host tool may be named `witness` without becoming acceptance evidence.
  The projection's evidence is folded from the events that recorded it.

## Tasks and authority

```
aja:  open -> running -> waiting -> done | failed
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
approval the host recorded **for this turn** — `TurnOptions.approvals` is written
as Approval events before the first decision — and a proposal from an earlier
turn. Model output alone cannot authorize confirmation. One identity event
applies the change and resolves the proposal atomically. An approval authorizes
one turn, so a proposal that needs one ends the turn and the next turn carries
it: the turn is the unit of waiting, not a mid-turn park.

EJA may observe any host-granted read-only capability. Its ability to observe
never depends on unrelated tasks. Effects occur through an AJA's host gateway.
MCP read-only declarations are scoped to their server and tool.

## Reading and continuity

```
look {source: log | aja | world, target, selector}
```

- Log targets: `task:<id>`, `event:<id>`, `history`, `all`. Discovery uses event
  indices with `range`; whole returns the latest 40 matching events. Pages expose
  `from`, `next`, `total` and `truncated`. Oversized events expose their id in
  `partial_events`; `event:<id>` supports byte views so paging always advances.
- Aja targets: `<id>/context.jsonl`, `<id>/aja.json`, `<id>/<workspace-file>`.
  Whole, range and match use the same source resolver, including checkpoints.
- World targets: `read_file`, `list_dir`, or a granted read-only connector.
  World tools select content through their own arguments.
- A target may instead be an address: `log:event:12`, `log:task:1`,
  `record:3/report.md` (one frozen file of aja 3), `world:src/out.txt` (one
  path of the project). One selector vocabulary applies to the space each
  authority is: a log page takes whole or range (an oversized event also takes
  bytes), a frozen record file takes whole, range or match, and a world address
  names one path and takes none. A target that is not an address is a host read
  tool, which selects through its own arguments.

Selectors are `{"select":"whole"}`, `{"select":"range","from":0,"lines":40}`,
`{"select":"match","text":"passage","before":2,"after":4}`, and
`{"select":"bytes","from":0,"bytes":4096}` for a serialized log event.
History is projected newest first; omitted history remains discoverable by id or
page. Views report truncation and count records. The rendered brain context
never exceeds its declared UTF-8 byte budget, including notices and headers.

## Fold

One renderer produces every view the model sees, for EJA and for a running aja
alike. A view is a space of regions in priority order — the newest step first,
the current input first, evidence before the index — and the renderer chooses a
scale for each region inside the declared budget:

| Scale | What the model gets |
|---|---|
| record | the region whole |
| index | one line, plus the address that reaches it |
| map | one line for a folded range of regions |

The head stays whole while the budget allows, the rest is listed at index scale,
and the oldest folds into one address range. Every region carries the address
that reads it back — `log:event:12`, `log:task:3`, or the record entry an aja
reads with `eja_read_record` — so the notice and the lines agree, and reading a
folded region again is an address lookup. Nothing is summarized by a model and
nothing is deleted. A aja whose transcript no longer fits even at map scale
**stops** (`ended: budget`) and continues in a new aja; it never keeps
re-sending a transcript pinned at the cap.

## Execution and resources

A turn runs `project -> decide -> act -> record` until a final decision, repeated
projection, cancellation, or a host budget. `TurnEnd` distinguishes these cases.
Creating a aja, group or proposal returns its allocated id through the next
projection, so the model never needs to guess a new id. Budget/cancellation results are explicit; intermediate replies are never final.
Every reserved running task is reconciled when execution stops.

Default host budgets:

| Resource | Default |
|---|---:|
| Turn decisions / actions / dispatches / slots | 128 / 4096 / 128 / 1 |
| Turn elapsed time | 30 minutes |
| Aja model steps / elapsed time | 256 / 300 seconds |
| Aja context / model response | 252 KiB / 64 KiB |
| IPC JSON payload | 256 KiB |
| One tool-result working view | 8 KiB, including address |
| Record passage read | at most 1 KiB of UTF-8 text |
| Brain rendered context | 200,000 bytes |

One `Control` hierarchy propagates the turn deadline/cancellation to the brain,
ajas, observations and model/tool calls. A child may shorten the deadline, never
extend it. Every port requires this control explicitly; there is one entry point
per operation. The one exception is cleanup: releasing a world runs after the
run's budget may have expired, so the backend declares that bound itself rather
than inheriting a spent control.

Dispatches run in action order. With one slot they also run one at a time in one
shared world; with more slots the host must reserve an isolated world per run, and
the journal still records reports in dispatch order. The turn owns how many runs a
decision starts; the world backend owns how many may exist at once, because it is
the only thing that knows whether a world is isolated. A refused world refuses the
run before it costs an aja id, and never reports itself as a budget stop. The host
owns aja step and time limits; the disposable loop only manages its bounded
message view.

## Recovery

A newline commits an event. Appends are serialized, flushed and synced. An
uncommitted final fragment is discarded under the writer lock before more
appends; malformed committed records are errors. Stale Store instances refuse
to append. The journal allows only one active turn, including across separate Store instances. These guards fail closed; they
are not a distributed multi-writer system.

Interrupted ajas are never dispatched again under the same id. Their task
becomes waiting for judgment, and continuation uses a new id. Frozen records
retain the complete safe prefix of an interrupted tool-call group. Damaged or
missing predecessor records are explicit errors, never a silent fresh start.

## Compatibility

0.14 gives each fact one owner: a projection borrows the fold instead of copying
it, one record module owns the manifest, the path resolver, the reader and the
shadow repository that freezes a run, a task that executes carries its run facts
in one type a group does not have, and the three bounded address spaces admit and
stop through one budget helper. A committed observation this version cannot decode
is counted in the evidence section rather than dropped. 0.13 logs stay readable:
a task without a run is a group, and an unreadable observation is a count.

0.13 makes the fold a value: a turn reads the journal once and pushes every event
it commits into the same `State`, so one derivation serves validation and the
projection. Evidence is that fold's output instead of a second copy a caller
supplies, an observation carries a typed kind instead of a reserved target string,
and every folded region names the address that reads it. The world backend is the
only concurrency guard, a refused world costs no aja id, and the record keeps its
frozen transcript when it is replayed. 0.12 logs stay readable: new fields default
to absent.

0.12 turns approvals into events, moves acceptance evidence into the task's own
contract, adds a slot budget with isolated worlds and world reconciliation, and
replaces the look source taxonomy with one address space. Older logs stay
readable: new fields default to absent, and a proposal without an Approval event
simply stays pending. A state directory that predates the `ajas/` layout (it has
`nodes/`) is refused by name; there is no migration.

0.11 renames the execution record from `nodes/` to `ajas/` and the environment
verbs `EJA_NODE_*` to `EJA_AJA_*`.

## Run

```bash
export LNA_BASE_URL=https://your-provider/v1
export LNA_API_KEY=...
export LNA_MODEL=...
# Optional aja endpoint/model: EJA_AJA_BASE_URL, EJA_AJA_API_KEY, EJA_AJA_MODEL

cargo run -- --dir /tmp/eja turn "draft a release checklist"
cargo run -- --dir /tmp/eja show tasks
cargo run -- --dir /tmp/eja show task:1
cargo run -- --dir /tmp/eja show aja:1
cargo run -- --dir /tmp/eja show audit:1
cargo run -- --dir /tmp/eja show context
cargo run -- --dir /tmp/eja --confirm 12 turn "apply the approved proposal"
```

Configure connectors in `<dir>/mcp.json`, or pass `--mcp-config FILE`.
`eja --dir DIR show tools` lists connector grants without model credentials.
Without connectors, a aja has its record reader and can produce a reply; it
has no filesystem-write or external-effect capability. See [MCP setup](docs/MCP.md)
and the [native protocol](docs/AJA-PROTOCOL.md).

Host resource configuration: `EJA_TURN_DECISIONS`, `EJA_TURN_ACTIONS`,
`EJA_TURN_DISPATCHES`, `EJA_AJA_SLOTS`, `EJA_TURN_TIMEOUT_MS`, `EJA_AJA_STEPS`,
`EJA_AJA_TIMEOUT_MS`, `EJA_CONTEXT_BYTES`. Limits must be positive.
`EJA_AJA_SLOTS` is the number of runs that may share a turn. With one slot the
host runs one shared world. Above one it needs a git project with at least one
commit, because each run gets its own worktree and branch (`<dir>/worlds/<id>`)
and is reconciled only when asked. A worktree world starts at `HEAD`: uncommitted
project changes stay in the project and are reported by the run's own account.
`--task ID` selects initial focus. `show audit:ID` follows explicit containment
and continuation edges; matching input text never invents a job relationship.

## Validation and limits

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

The crate holds itself to clippy's pedantic and nursery groups, declared in
`Cargo.toml`. Two lints are named there instead of ignored at each site:
`missing_errors_doc` (every fallible function states its refusals in the prose of
the contract, not in a generated list) and `too_many_lines` (the turn loop and
the projection are each one sequence a reader follows top to bottom).

Every integration test builds its scenario with `tests/support/`: one temporary
state, one scripted brain, one worker, one turn. A test body is a construction
and its assertions, never its own copy of the harness. A test that only restates
what another test already proves, or what the type system already guarantees, is
deleted rather than maintained.

Composition regressions cover unrelated-task invariance, large-result replay,
record addressability, aggregate completion, relation uniqueness and cross-kind
cycles, host authorization, atomic confirmation, torn-tail recovery, UTF-8 byte
budgets, deadlines and always-changing model states. Identity regressions add the
protocol-only contract, self-before-index rendering and addressable folding.
Acceptance regressions add one-turn approvals and witness-or-undecidable
judgments; world regressions add parallel slots with ordered reports, explicit
reconciliation and conflict reporting. Native/MCP tests also cover
real local HTTP, cancellation, process cleanup and frozen checkpoints, including
a record repository that asks git for a hostile hook and a signature the run
does not have.

Trusted adapters must honor Control, own their resources and bound their output.
An ordinary child process is not an OS security sandbox. Cancellation cannot
undo remote side effects. The kernel guarantees bounded host execution under
these port contracts; it cannot prove a model will solve an arbitrary objective.
Identity and task state can be replayed; world effects are never replayed by
folding the log. Legacy records/logs remain readable; past invalid relationships
are not retroactively authorized, and dispatch refuses ambiguous ancestry.

Historical, pre-0.11 model measurements are kept separately in
[benchmark history](docs/BENCHMARKS.md); they are not measurements of this revision.
