# Architecture contract

This document is the normative contract. README is an operational summary;
protocol and MCP documents specify their boundaries, not additional authorities.

## Minimal model

```
P = (L, R, K)                         persistent facts and evidence
S = fold(L)                          logical state
C = view_B(S, input)                  model's bounded working view
A = model(C)                         proposed typed actions
(O, W') = execute(A, W, grants, ctrl) actual observations and external effects
L' = L ++ events(A, O)               recorded transition
```

`L` owns identity, task and judgment facts. `R` owns raw execution evidence;
`K` owns optional frozen workspace evidence. The mutable world `W` is external.
Every view points at one **address** in one authority — `log:<target>`,
`record:<aja>/<path>`, `world:<path>` — and any target that is not an
address is a host read tool. Folding and reading use that one namespace.
One fact has one authority; keeping a derived index or snapshot does not create
another authority. Folding never re-executes world effects.

`fold` is one value, not a library of scans. `State` applies the journal event by
event — the initial `fold` and every append inside a turn run the same transition
— so a decision is validated against the state it will produce, the journal is
read once per turn, and the projection is a read of that value. Every derived
thing (task table, graph, pending proposals, the turn's evidence) is a field or a
trivial read of it, never a second pass that could disagree.

## Five invariants

1. **Facts and projections.** Events and raw records are immutable facts. Task
   tables, relations, pending proposals, digests and model contexts are derived.
   An assignment freezes a aja's activation contract; it is not a second task.
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
   match, and a world address names one path and takes none. Continuation stores
   parent references, reconstructs a valid prefix, applies the budget before
   transport, and uses the new assignment and new system budget. Views never
   change facts.
4. **One execution meaning.** An aja executes once; groups aggregate and never
   execute. Continuation has one predecessor. Dependency and containment rules
   are checked against one combined completion-order graph before logging.
5. **Owned finite execution.** Every turn has decision/action/dispatch/time
   budgets and a slot budget. Every aja has its own time/context/response budget.
   One Control hierarchy can only shorten authority's deadline. Each execution
   reserves one world and owns one process. The turn owns how many runs a decision
   may start; the world backend owns how many may exist at once, because it is
   the only thing that knows whether a world is isolated: one shared world has one
   owner, and refusing a world refuses the run before it costs an aja id. Releasing
   a world is cleanup, so it runs under a bound the backend declares, never under
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
| Aja | reconcile | after a report; the host lands the run's world or reports conflicts |
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
Old records whose start already contains inherited history are ancestry roots.

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
current world observations.

Checkpoint file reads and native record reads share one source resolver for
whole/range/match. Readers bound retained bytes and return truncation metadata.
Log pages include their next cursor in model evidence. An oversized event is
represented by an explicit locator, never a modified fact; bounded UTF-8 byte
views of `event:<id>` preserve access without blocking later page entries.
The shadow Git repository captures Git-visible files, excludes EJA state and
`.git`, disables hooks/signing/global configuration and runs under host Control.
Checkpoint failure is recorded explicitly; it cannot erase an already frozen
transcript or manufacture an acceptance.

## Port contracts

- `Brain::decide`: return a decision under the supplied Control.
- `Worker::execute`: own one run in its reserved world; return its observed
  digest. Runs execute in action order; each completed report is recorded
  before the next.
- `Host::look`: read only, bound output, respect Control; one address space.
- `Host::witness`: the host's own observation of declared acceptance evidence.
  A host that declares none is still complete: the default refuses, so an
  acceptance is unavailable and only a rejection is left open.
  `Host::reconcile`: land one run's world, or report its conflicts as the
  recorded outcome.
- `Worlds`: reserve, settle, reconcile and release the world one execution may
  change. A world is a lease — where a run may write — and the backend keeps the
  reservation that turns it into an account: one line, measured, complete or
  explicitly unobserved. Isolation is the host's decision; the kernel never
  assumes a repository. Reserve is also the concurrency guard: a backend that
  cannot isolate refuses, so no second mechanism counts owners and a refused
  world costs no aja id. Release is cleanup, so it runs under a bound the backend
  declares rather than under the spent control of the run it cleans up after.
- `Model` and `ToolGateway`: host effects under inherited grants/control.
- `RecordReader` / `Capabilities`: what the host lets EJA read or witness,
  composed by Ports, without another state store or task authority.

Every operation requires an explicit Control, including aja-file selection,
world observation and connector startup. There is no uncontrolled entry point or
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
| Parallel slots run at once and the log keeps action order | worlds |
| One world, one owner: a second run is refused before it costs an aja id | worlds |
| A worktree world settles and reconciles explicitly; conflicts are reported | worlds |

Repeated projections detect stalls. They are not a convergence proof. The strict
execution bound comes from the host budgets; solving a natural-language objective
remains model-dependent.

## Ownership and compatibility

Core modules are `domain`, `store`, `state`, `turn`, the shared `control`
and the one renderer `fold`. `state` is the fold itself: `S = fold(L)` is a
value the turn maintains by pushing each event it commits, so the journal is read
once per turn and every derived table is a field of that value.
Adapters never obtain a mutable identity handle. Store's low-level append API is
for trusted host/admin code, including fixture construction. The journal-wide
Unix turn lease is the sole turn exclusion guard. Append locking and stale-cursor
checks protect the rebuildable cursor; they do not implement a distributed scheduler.

Existing serialized ajas default to kind `aja`; grouping is never guessed
from text or old containment usage. Legacy confirmation events still resolve
legacy proposals. A legacy ambiguous continuation is refused at dispatch. Prior
historical facts are not rewritten to claim they met rules introduced later.