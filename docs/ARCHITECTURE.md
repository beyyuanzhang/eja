# Architecture contract

This document is the normative contract. README is an operational summary;
protocol and MCP documents specify their boundaries, not additional authorities.

## Minimal model

```
P = (L, R, K)                         persistent facts and evidence
S = fold(L)                          logical state
C = view_B(S, input, evidence(P))     model's bounded working view
A = model(C)                         proposed typed actions
(O, W') = execute(A, W, grants, ctrl) actual observations and external effects
L' = L ++ events(A, O)               recorded transition
```

`L` owns identity, task and judgment facts. `R` owns raw execution evidence;
`K` owns optional frozen workspace evidence. The mutable world `W` is external.
One fact has one authority; keeping a derived index or snapshot does not create
another authority. Folding never re-executes world effects.

## Five invariants

1. **Facts and projections.** Events and raw records are immutable facts. Task
   tables, relations, pending proposals, digests and model contexts are derived.
   An assignment freezes a node's activation contract; it is not a second task.
2. **Explicit authority.** Typed ids and host grants authorize transitions.
   Model text does not grant approval. Confirmation requires a recorded host
   approval for that turn and a pending earlier proposal. One event applies and
   resolves it. Goal/commitment replacement closes an active same-kind id;
   correction only withdraws an active goal/commitment id.
3. **Bounded, addressable views.** One renderer produces every model-facing
   view: EJA's projection and a node transcript are the same call over different
   spaces (the log; the node's own record). Regions are shown whole while the
   declared budget allows, one line plus handle after that, and the oldest fold
   into one address range; a folded region stays addressable, so unfolding is a
   read, never a re-derivation. Full payloads remain addressable. Continuation
   stores parent references, reconstructs a valid prefix, applies the budget
   before transport, and uses the new assignment and new system budget. Views
   never change facts.
4. **One execution meaning.** Nodes execute once; groups aggregate and never
   execute. Continuation has one predecessor. Dependency and containment rules
   are checked against one combined completion-order graph before logging.
5. **Owned finite execution.** Every turn has decision/action/dispatch/time
   budgets. Every node has its own time/context/response budget. One Control
   hierarchy can only shorten authority's deadline. Native execution owns one
   process and one mutable workspace at a time. Stop reasons and evidence survive
   cleanup; recovery never reruns a node.

Protocol-independent work tactics remain model decisions. A global waiting-task
flag is not an observation capability. Text equality is not a task relationship.

The current goal is the newest unclosed Goal event. Adding a goal without a
replacement target leaves prior goals active; withdrawing the newest can reveal
an older active goal. Explicit target_id replacement closes its predecessor.
This selection rule is a projection of ids, never phrase matching. Identity notes
(decisions, relationships and correction reasons) are advisory history, not
another approval channel.

## State transitions

| Entity | Operation | Preconditions / result |
|---|---|---|
| Node | open_task | Open, no execution yet |
| Node | dispatch | Open; depends_on targets Done; continues target has a frozen-run report; then Running |
| Node | worker report / recovery | Running -> Waiting |
| Node | close_task | Waiting -> Done or Failed, EJA's judgment |
| Node | close_task accepted:false | Open -> Failed, cancellation before execution |
| Group | open_group | Open, never dispatched |
| Group | close_task accepted:true | Every child terminal; EJA accepts the aggregate contract; then Done |
| Group | close_task accepted:false | Every child terminal; then Failed |

`part_of` has one parent and targets an open group. A group depends on the
completion of its children. `depends_on` and `continues` are fixed before their
source node dispatches. `continues` targets a reported node, with exactly one
predecessor. `depends_on` may target a node or a group. Both enter the combined
completion-order graph with reversed containment edges, so cross-kind cycles
are refused. Descriptive edges do not change execution state.

Creation is an observation of an allocated id: the next projection lists every
new node/group in its task index (the last one created in that decision also
becomes the focus) and every new proposal in the pending index. A caller can
then address them without guessing ids. Open -> dispatch is reserved in action order; the actual worker runs after the
other actions in that decision. Therefore a continuation is opened with
run:false, related to its predecessor, then dispatched. A later relation cannot
retroactively change the assignment. Identical relation insertion is idempotent.

## Records and views

A v1 start record contains `{kind:"start", view_version:1, messages}`;
messages contain only this node's system and assignment. The frozen
`assignment.resume` is the sole parent address. Tool entries include
provider call ids, arguments and full results. The record's zero-based entry
index and node id form a stable address.

Reconstruction follows immutable parent manifests, rejects cycles/missing
records, and folds entries oldest to newest through the same renderer. An unfinished tool group contributes its complete earlier prefix, not an
invalid provider message sequence. Its partial raw effects remain in the record.
Old records whose start already contains inherited history are ancestry roots.

`eja_read_record` may read this node or its explicit ancestors, never arbitrary
host state. Live model messages use the current system contract even when old
messages are inherited.

The fold states its landing scale in the rendered view only when the budget
forced something out; a view that fits whole carries no notice. A node whose
transcript cannot fit even as one address line stops with `budget` and continues
in a new node, so a transcript is never re-sent pinned at the cap. No checkpoint is restored during continuation: historical
observations must be distinguished from current world observations.

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
- `Worker::execute`: own and settle one run; return its observed digest. Runs
  execute in action order; each completed report is recorded before the next.
- `Lookup::look`: read only, bound output, respect Control.
- `Model` and `ToolGateway`: host effects under inherited grants/control.
- `NodeReader` / `Observer`: capabilities composed by Ports, without another
  state store or task authority.

Every operation requires an explicit Control, including node-file selection,
world observation and connector startup. There is no uncontrolled entry point or
fallback implementation. Native/model/MCP/project readers honor this contract.
External adapters are trusted code; their cancellation contracts govern remote effects.

Shared text views bound both bytes read and encoded output. A failed node-file
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

Repeated projections detect stalls. They are not a convergence proof. The strict
execution bound comes from the host budgets; solving a natural-language objective
remains model-dependent.

## Ownership and compatibility

Core modules are `domain`, `store`, `state`, `turn`, and shared `control`.
Adapters never obtain a mutable identity handle. Store's low-level append API is
for trusted host/admin code, including fixture construction. The journal-wide
Unix turn lease is the sole turn exclusion guard. Append locking and stale-cursor
checks protect the rebuildable cursor; they do not implement a distributed scheduler.

Existing serialized nodes default to kind `node`; grouping is never guessed
from text or old containment usage. Legacy confirmation events still resolve
legacy proposals. A legacy ambiguous continuation is refused at dispatch. Prior
historical facts are not rewritten to claim they met rules introduced later.
