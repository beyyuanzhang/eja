# Architecture contract

This document is the normative contract. README is an operational summary;
protocol and MCP documents specify their boundaries, not additional authorities.

## Minimal model

```
P = (L, R, K)                         persistent facts and evidence
S = fold(L)                          logical state
C = view_B(S, input)                  model's bounded working view
A = model(C)                         proposed typed actions
(O, E') = execute(A, E, grants, ctrl) actual observations and external effects
L' = L ++ events(A, O)               recorded transition
```

`L` owns identity, task and judgment facts. `R` owns raw execution evidence;
`K` owns optional frozen workspace evidence. The mutable environment `E` is external.
Every view points at one **address** in one authority — `log:<target>`,
`record:<aja>/<path>`, `environment:<path>` — and any target that is not an
address is a host read tool. Folding and reading use that one namespace.
One fact has one authority; keeping a derived index or snapshot does not create
another authority. Folding never re-executes environment effects.

`fold` is one value, not a library of scans. `State` applies the journal event by
event — the initial `fold` and every append inside a turn run the same transition
— so a decision is validated against the state it will produce, the journal is
read once per turn, and the projection is a read of that value. Every derived
thing (task table, graph, pending proposals, the turn's evidence) is a field or a
trivial read of it, never a second pass that could disagree.

`C` is a view, not a copy. It borrows the facts it selects, the derived tables it
shows and the input it is projected with — `EjaContext<'_>` holds references, so
a decision allocates a selection of pointers, never a second copy of the log. A
turn's outcome owns `S` and derives `C` on demand, so one turn holds one copy of
its facts and no projection outlives the facts it describes. A view that must
outlive its fold would be a second authority, so there is none.

## Five invariants

1. **Facts and projections.** Events and raw records are immutable facts. Task
   tables, relations, pending proposals, digests and model contexts are derived.
   An assignment freezes a aja's activation contract; it is not a second task.
   A projection borrows the fold; the fold is the only copy of a fact. An outcome
   owns the fold it produced, and `context()` derives the view from it.
2. **Explicit authority.** Typed ids and host grants authorize transitions.
   Model text does not grant approval: an approval is its own event, recorded by
   the host for one turn, and confirmation needs it plus a pending proposal from
   an earlier turn. One event applies and resolves the proposal. Goal/commitment
   replacement closes an active same-kind id; correction only withdraws one. A
   turn is the unit of waiting: a proposal that needs an approval ends the turn
   and the next turn carries it, so no journal lease is held for a human.
   A judgment counts only on evidence the host observed, not on the runner's
   account: acceptance needs it, and a rejection does not — so evidence that
   cannot be obtained cannot strand a task.
3. **Bounded, addressable views.** One renderer produces every model-facing
   view: EJA's projection and a aja transcript are the same call over different
   spaces (the log; the aja's own record). Every region carries the address that
   reads it back — `log:event:12`, `log:task:3`, or the record entry an aja
   reads with `eja_read_record`. Regions are shown whole while the declared
   budget allows, one line with that address after that, and the oldest fold into
   one address range; unfolding is a read, never a re-derivation. A region that
   names no address says so instead of promising a handle. Full payloads remain
   addressable. One selector vocabulary — whole, range, match, bytes — is applied
   to the space each authority is: a log page takes whole or range (and an
   oversized event also takes bytes), a frozen record file takes whole, range or
   match, and an environment address names one path and takes none. Continuation stores
   parent references, reconstructs a valid prefix, applies the budget before
   transport, and uses the new assignment and new system budget. Views never
   change facts.
4. **One execution meaning.** An aja executes once; groups aggregate and never
   execute. Continuation has one predecessor. Dependency and containment rules
   are checked against one combined completion-order graph before logging.
5. **Owned finite execution.** Every turn has decision/action/dispatch/time
   budgets and a slot budget. Every aja has its own time/context/response budget.
   One Control hierarchy can only shorten authority's deadline. Each execution
   reserves one environment and owns one process. The turn owns how many runs a decision
   may start; the environment provider owns how many may exist at once, because it is
   the only thing that knows whether an environment is isolated: one shared environment has one
   owner, and refusing an environment refuses the run before it costs an aja id. Releasing
   an environment is cleanup, so it runs under a bound the provider declares, never under
   the spent control of the run it cleans up after. Stop reasons and evidence
   survive cleanup; recovery never reruns a aja.

Protocol-independent work tactics remain model decisions. A global waiting-task
flag is not an observation capability. Text equality is not a task relationship.

The current goal is the newest unclosed Goal event. Adding a goal without a
replacement target leaves prior goals active; withdrawing the newest can reveal
an older active goal. Explicit target_id replacement closes its predecessor.
This selection rule is a projection of ids, never phrase matching. Identity notes
(decisions, relationships and correction reasons) are advisory history, not
another approval channel.

## Identity

The subject is not described to the model: it is read back from its own record.
The contract states the protocol — one JSON decision, the action shapes, the
preconditions, the budgets — and never states who the subject is or how it
should feel. Goal, commitment, correction, decision and relationship events are
the only source of self-description, and the projection puts them before the
indexes so the self is never the first thing a budget folds away.

Three properties are testable, and `tests/identity.rs` holds them:

| Property | Test |
|---|---|
| Addressable | any folded region reads back through its address |
| Self-sourced | the contract contains no claim about the subject's character |
| Judgeable | a declared witness needs host evidence before a judgment |

## State transitions

| Entity | Operation | Preconditions / result |
|---|---|---|
| Aja | open_task | Open, no execution yet; `evidence` fixes the observation its judgment needs |
| Aja | dispatch | Open; depends_on targets Done; continues target has a frozen-run report; then Running |
| Aja | worker report / recovery | Running -> Waiting |
| Aja | close_task | Waiting -> Done or Failed, EJA's judgment; needs the witness evidence when `evidence` was declared |
| Aja | reconcile | after a report; the host lands the run's environment or reports conflicts |
| Aja | close_task accepted:false | Open -> Failed, cancellation before execution |
| Group | open_group | Open, never dispatched |
| Group | close_task accepted:true | Every child terminal; EJA accepts the aggregate contract; then Done |
| Group | close_task accepted:false | Every child terminal; then Failed |

`part_of` has one parent and targets an open group. A group depends on the
completion of its children. `depends_on` and `continues` are fixed before their
source aja dispatches. `continues` targets a reported aja, with exactly one
predecessor. `depends_on` may target a aja or a group. Both enter the combined
completion-order graph with reversed containment edges, so cross-kind cycles
are refused. Descriptive edges do not change execution state.

Creation is an observation of an allocated id: the next projection lists every
new aja/group in its task index (the last one created in that decision also
becomes the focus) and every new proposal in the pending index. A caller can
then address them without guessing ids. Open -> dispatch is reserved in action order; the actual worker runs after the
other actions in that decision. Therefore a continuation is opened with
run:false, related to its predecessor, then dispatched. A later relation cannot
retroactively change the assignment. Identical relation insertion is idempotent.

## Records and views

An observation that this version cannot decode is counted, not dropped: the
evidence section states how many committed observations it could not show.

An observation says what it is — `look`, `witness` or `reconcile` — and its
target then names the subject. The kind is a typed field, never a reserved target
string: a host read tool may be named `witness`, and a fact may not depend on
that collision staying unambiguous. A look that repeats keeps the newest answer
in the projection, so the same read twice states the same information.

A v1 start record contains `{kind:"start", view_version:1, messages}`;
messages contain only this aja's system and assignment. The frozen
`assignment.resume` is the sole parent address. Tool entries include
provider call ids, arguments and full results. The record's zero-based entry
index and aja id form a stable address.

Reconstruction follows immutable parent manifests, rejects cycles/missing
records, and folds entries oldest to newest through the same renderer. An unfinished tool group contributes its complete earlier prefix, not an
invalid provider message sequence. Its partial raw effects remain in the record.
Every record read requires the current `eja.aja` format, version 1, and a v1
start entry.

`eja_read_record` may read this aja or its explicit ancestors, never arbitrary
host state. Live model messages use the current system contract even when old
messages are inherited.

The fold states its landing scale in the rendered view only when the budget
forced something out; a view that fits whole carries no notice. A region shown at
index scale names the address range that unfolds it, so the notice and the lines
agree about how to reach the rest: a projection leaves its facts readable by id,
and a transcript leaves them readable as record entries. A aja whose transcript
cannot fit even as one address line stops with `budget` and continues in a new
aja, so a transcript is never re-sent pinned at the cap. No checkpoint is
restored during continuation: historical observations must be distinguished from
current environment observations.

Checkpoint file reads and native record reads share one source resolver for
whole/range/match, and one record module owns the manifest type, the resolver,
the reader and the shadow repository that freezes a run. Readers bound retained
bytes through the same budget helper and return truncation metadata.
Log pages include their next cursor in model evidence. An oversized event is
represented by an explicit locator, never a modified fact; bounded UTF-8 byte
views of `event:<id>` preserve access without blocking later page entries.
The shadow Git repository captures Git-visible files, excludes EJA state and
`.git`, disables hooks/signing/global configuration and runs under host Control.
The record store owns that repository; a worker hands it a run and receives a
commit, so no adapter keeps a second copy of the record layout.
Checkpoint failure is recorded explicitly; it cannot erase an already frozen
transcript or manufacture an acceptance.

## Composition

`host::capability` is the one flat capability catalog. Built-ins and installed
plugins publish the same card shape: id, title, description, service kind,
default state and private config. `host::compose` is the single assembly root for
CLI and HTTP. It reads `app-settings.json`, walks enabled cards, assembles their
providers and returns one `ComposedHost` with record-store services and
capability diagnostics. The kernel has no capability enum, plugin list or
workspace/Git/MCP variant.

The catalog supplies services to existing contracts. A tool card contributes a
`ToolProvider`, an environment card contributes an `EnvironmentProvider`, and a
background card contributes host-owned services. The catalog itself has no
dispatch protocol and adds no kernel authority.

Tools and environments have different contracts because they answer different
questions. `ToolProvider` declares callable actions and their names.
`ToolRegistry` routes a name to the provider that owns it. Workspace files, shell,
MCP servers and managed plugins are peers at that boundary. Each provider owns its
protocol, path rules, grants and output bound.

`EnvironmentProvider` owns the lifecycle of a mutable execution environment. It
reserves exclusive access, measures changes at settlement, reconciles a recorded
result and releases the lease. Shared directories and injected Git worktrees
implement the same lifecycle contract. A future container or remote database
environment can implement it without adding a new kernel action.

`ExecutionServices` is a constructor bundle passed to `NativeWorker`. It groups
the selected workspace, checkpoint store and environment provider so one worker
can execute a run. It has no dispatch protocol and adds no kernel authority.

Git appears in two external adapter roles. `Checkpoints` freezes Git-visible
evidence for records. `GitWorktrees` supplies isolated execution environments and
durable branches. They are separate `checkpoints` and `git-worktrees` cards, so
either service can run without the other. The kernel sees neither role directly
and contains no repository topology.

Managed plugins are external tool cards. Their manifest supplies card metadata
and default config, the active pointer versions the code, and the manager stages,
tests, activates and rolls back versions. A plugin receives its config through
`EJA_PLUGIN_CONFIG`; it still receives no workspace, host credential or network
authority. Third-party tools enter through MCP and therefore use the same
`ToolProvider` boundary as built-in files, shell and connectors.

The HTTP host keeps one `host::state::Cache` derived from the journal. It
rechecks the file stamp, folds only after a change, and serves event tails after
the caller's cursor. The cache is rebuildable and never becomes a second
authority. MCP connector types and normalization remain an adapter-owned config
detail behind the generic card storage.

## Port contracts

- `Brain::decide`: return a decision under the supplied Control.
- `Worker::execute`: own one run in its reserved environment; return its observed
  digest. Runs execute in action order; each completed report is recorded
  before the next.
- `Host::look`: read only, bound output, respect Control; one address space.
- `Host::witness`: the host's own observation of declared acceptance evidence.
  A host that declares none is still complete: the default refuses, so an
  acceptance is unavailable and only a rejection is left open.
  `Host::reconcile`: land one run's environment, or report its conflicts as the
  recorded outcome.
- `EnvironmentProvider`: reserve, settle, reconcile and release the environment one execution may
  change. An environment lease says where a run may write, and the provider keeps the
  reservation that turns it into an account: one line, measured, complete or
  explicitly unobserved. Isolation is the host's decision; the kernel never
  assumes a repository. Reserve is also the concurrency guard: a backend that
  cannot isolate refuses, so no second mechanism counts owners and a refused
  environment costs no aja id. Release is cleanup, so it runs under a bound the provider
  declares rather than under the spent control of the run it cleans up after.
- `Model` and `ToolProvider`: host effects under inherited grants/control.
- `RecordReader` / `Capabilities`: what the host lets EJA read or witness,
  composed by Ports, without another state store or task authority.

Every operation requires an explicit Control, including aja-file selection,
environment observation and connector startup. There is no uncontrolled entry point or
fallback implementation. Native/model/MCP/project readers honor this contract.
External adapters are trusted code; their cancellation contracts govern remote effects.

Shared text views bound both bytes read and encoded output. A failed aja-file
read uses the same Result/Observation error path as every other lookup. Match
windows are derived from ordered hit positions, not a second pending-hit state.

## Composition laws and tests

| Law | Regression |
|---|---|
| Adding an unrelated task does not change observation capabilities | convergence + observe |
| A completed large record can be resumed through a bounded view | native_worker |
| Live/replay tool views are identical for the same record address | native_worker |
| A truncated payload can be read by its address | native_worker |
| A continuation stores no duplicate raw ancestor payload | native_worker |
| Groups finish without a dummy execution | convergence |
| No multiple predecessor, dangling id, self edge or combined dependency cycle | convergence |
| Allocated ids are observable before subsequent actions address them | convergence |
| Model confirmation without host approval cannot alter identity | convergence + proposals |
| Confirmation has one atomic resolving event | convergence |
| Recovering a torn tail allows later appends; committed corruption errors | convergence + recovery |
| Cloned/independent Store instances cannot recover each other’s live turn | convergence |
| New states cannot bypass a finite turn budget | convergence |
| Rendered contexts respect exact UTF-8 byte bounds | convergence + projection |
| The contract states no character, and goal/identity render before indexes | identity |
| A folded region reads back through its address | identity |
| A host approval authorizes only the turn it was recorded in | acceptance |
| Acceptance needs observed evidence; a rejection needs none | acceptance |
| A repeated read states the same information, so a turn ends at its fixpoint | turn_steps |
| The turn's evidence is folded from the journal, never supplied by a caller | projection |
| A folded region names an address that reads it | fold + identity |
| Parallel slots run at once and the log keeps action order | environments |
| One shared environment, one owner: a second run is refused before it costs an aja id | environments |
| A Git worktree environment settles and reconciles explicitly; conflicts are reported | environments |

A decision reports whether it added information: a fact the projection states, or
a refusal told for the first time. A look that repeats the observation it would
replace added none, and the turn ends at its fixpoint. The kernel decides that
from what each action produced, not by keeping a second projection to compare
against — a comparison of borrowed views is impossible by construction, and it
was never the right question: what matters is whether the decision told the
subject anything it did not already know.

Stall detection is not a convergence proof. The strict execution bound comes from
the host budgets; solving a natural-language objective remains model-dependent.

## Ownership

Core modules are `domain`, `store`, `state`, `turn`, the shared `control`
and the one renderer `fold`. `state` is the fold itself: `S = fold(L)` is a
value the turn maintains by pushing each event it commits, so the journal is read
once per turn and every derived table is a field of that value.
Adapters never obtain a mutable identity handle. Store's low-level append API is
for trusted host/admin code, including fixture construction. The journal-wide
Unix turn lease is the sole turn exclusion guard. Append locking and stale-cursor
checks protect the rebuildable cursor; they do not implement a distributed scheduler.

An initialized state directory contains `state.json` with
`{"format":"eja","version":1}`. A non-empty directory without this marker, or
with another format or version, is refused. Execution manifests use
`{"format":"eja.aja","version":1}`. The runtime does not rewrite, upgrade or
fall back to an earlier layout. Prior historical facts are not rewritten to
claim they met rules introduced later.
