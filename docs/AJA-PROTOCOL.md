# Native AJA protocol

The same executable runs in private `--aja-worker` mode. It starts with an empty
environment in the world the host reserved for this run: the project itself when
there is one shared world, a worktree when slots are isolated. The host owns model credentials, MCP
connections, tool grants and all external effects. The aja only runs framed IPC.

## Startup

```
Start { context_bytes, messages }
```

The host resolves the explicit predecessor's frozen record ancestry, reconstructs
its valid working view, replaces the old system contract, adds the new assignment,
and fits the resulting messages before sending Start. The serialized JSON frame
is at most 256 KiB, followed by a newline. A fresh aja has system + assignment
messages; continuation adds historical observations, not restored world state.
The assignment is frozen once in the host manifest; its prepared messages cross
IPC without a second copy of the same brief. Step/time/response limits belong to
the host, not a mirrored execution counter in the child. The child receives only
the context bound needed to fit its working messages before transport.

## Requests

- `model {messages}`: host calls its configured model with snapshotted tools.
- `tool {id,name,arguments}`: host checks the grant and binds the real aja id,
  workspace/private roots and Control; the aja cannot expand them.
- `finish {text,reason}`: aja's final account; reason is message, budget or error.

The host records full assistant messages and full tool results, with provider
call ids. Each result returned to the aja is a view of at most 8 KiB, including
its `{aja,entry}` address. Small results use `{record,result}`; larger ones have
an explicit truncated head/tail. `eja_read_record {aja,offset,bytes}` reads
at most 1 KiB from a full result in this aja or its explicit ancestry.

Context fitting folds the transcript: the system prompt, the assignment, and the
newest steps stay whole while the budget allows, older steps become one address
line each, and the oldest fold into one address range. Nothing is deleted from
the record. Every step line names the record entry it came from, and the fold line
names the range of those entries, so a folded step is read back with
`eja_read_record {aja, entry}` — the record half of the same address space that
`log:` and `world:` name from the host side.
A contract that cannot fit even at that scale ends with budget. Incomplete
provider responses and invalid tool messages end with error.

## Frozen record

`context.jsonl` contains a v1 start entry, this run's assistant/tool entries and
finish. Start stores only fresh messages. The frozen assignment.resume is its parent address. A
continuation does not copy ancestor payloads. Replaying legacy records that
already contain inherited start messages treats them as ancestry roots.

Replaying a predecessor folds its transcript against the same budget, but only
when the working set outgrows it: measuring the whole transcript once per record
entry would cost the square of a long record for no new information.

`aja.json` is published by atomic rename after process cleanup and transcript
sync. It contains assignment, digest, optional checkpoint, checkpoint_error,
context file/size and frozen:true. Frozen files become read-only. Checkpointing
captures the Git-visible files of that world; whole/range/match read the same
commit.
Checkpoint failure remains visible and does not discard the execution record.

A record is evidence, not acceptance. EJA's later close_task event is the verdict.
Aja ids are reserved once and never rerun, including interrupted/unsealed ids.
A continuation uses a new aja id and a single explicit predecessor.

## Lifetime

Native execution runs in one reserved world per aja. With one slot that world is
the shared project, so runs are serial; with more slots each run gets its own
worktree and the host records reports in dispatch order. Host cancellation and
deadlines propagate through model requests and tool calls. The host kills/reaps the aja,
joins its IPC reader and revokes its run before publishing the frozen record.
Waiting for EJA judgment retains no AJA process. All cleanup and checkpoint
resources belong to the host. Ordinary filesystem permissions and child processes
are application boundaries, not an OS security sandbox.

If worktree settlement fails, including cancellation or timeout, its directory
is retained and the run's account reports its absolute recovery path. A settled
branch remains available for reconciliation and cannot be replaced by reusing an
aja id. Checkpoint writes sharing a store serialize the complete Git transaction.

MCP connections belong to the command/application, and are shared across the
ajas of one host. Cancelling a request asks the server to stop; it cannot undo remote effects.
Legacy host/admin adapters must satisfy the same bounded-output and Control
contracts to participate in finite execution.
