# Native AJA protocol

The same executable runs in private `--aja-worker` mode. It starts with an empty
environment in the project workspace. The host owns model credentials, MCP
connections, tool grants and all external effects. The node only runs framed IPC.

## Startup

```
Start { limits, messages }
Limits { timeout_ms, steps, context_bytes, response_bytes }
```

The host resolves the explicit predecessor's frozen record ancestry, reconstructs
its valid working view, replaces the old system contract, adds the new assignment,
and fits the resulting messages before sending Start. The serialized JSON frame
is at most 256 KiB, followed by a newline. A fresh node has system + assignment
messages; continuation adds historical observations, not restored world state.
The assignment is frozen once in the host manifest; its prepared messages cross
IPC without a second copy of the same brief.

## Requests

- `model {messages}`: host calls its configured model with snapshotted tools.
- `tool {id,name,arguments}`: host checks the grant and binds the real node id,
  workspace/private roots and Control; the node cannot expand them.
- `finish {text,reason}`: node's final account; reason is message, budget or error.

The host records full assistant messages and full tool results, with provider
call ids. Each result returned to the node is a view of at most 8 KiB, including
its `{node,entry}` address. Small results use `{record,result}`; larger ones have
an explicit truncated head/tail. `eja_read_record {node,entry,offset,bytes}` reads
at most 1 KiB from a full result in this node or its explicit ancestry.

Context fitting preserves the current system/assignment and valid tool-call
message groups. It elides payloads or complete old groups as needed; raw evidence
is never removed. A contract that cannot fit ends with budget. Incomplete provider
responses and invalid tool messages end with error.

## Frozen record

`context.jsonl` contains a v1 start entry, this run's assistant/tool entries and
finish. Start stores only fresh messages. The frozen assignment.resume is its parent address. A
continuation does not copy ancestor payloads. Replaying legacy records that
already contain inherited start messages treats them as ancestry roots.

`node.json` is published by atomic rename after process cleanup and transcript
sync. It contains assignment, digest, optional checkpoint, checkpoint_error,
context file/size and frozen:true. Frozen files become read-only. Checkpointing
captures Git-visible project files; whole/range/match read the same commit.
Checkpoint failure remains visible and does not discard the execution record.

A record is evidence, not acceptance. EJA's later close_task event is the verdict.
Node ids are reserved once and never rerun, including interrupted/unsealed ids.
A continuation uses a new node id and a single explicit predecessor.

## Lifetime

Native execution is serial over one mutable workspace. Host cancellation/deadline
propagates through model requests and tool calls. The host kills/reaps the node,
joins its IPC reader and revokes its run before publishing the frozen record.
Waiting for EJA judgment retains no AJA process. All cleanup and checkpoint
resources belong to the host. Ordinary filesystem permissions and child processes
are application boundaries, not an OS security sandbox.

MCP connections belong to the command/application, and are shared across serial
nodes. Cancelling a request asks the server to stop; it cannot undo remote effects.
Legacy host/admin adapters must satisfy the same bounded-output and Control
contracts to participate in finite execution.
