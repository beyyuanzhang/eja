# Native AJA protocol

An AJA is one disposable work node, identified by its task id. The same Rust
binary runs in private `--aja-worker` mode; no user-selectable worker program,
plugin loader, secondary worker protocol or automatic configuration discovery
exists.

## Startup

The host starts the process in its node work directory with an empty environment.
Only bounded JSON-lines IPC is available on stdin/stdout; stderr is discarded.
The initial frame contains:

```json
{
  "assignment": {"task_id": 1, "objective": "write the checklist",
                 "acceptance": "contains build, smoke test and upload",
                 "issued_at": 1762699200},
  "limits": {"timeout_ms": 300000, "steps": 16, "context_bytes": 262144, "response_bytes": 65536}
}
```

`objective` is the specific node request and `acceptance` is its contract: the
work is done when it holds. The EJA log, the original user history, credentials
and connector configuration are not sent.

## Requests

The node sends one request and waits for the host response:

- `model {messages}` — host calls its configured model with the run's tool definitions.
- `tool {name, arguments}` — host checks the snapshotted grant and binds the real
  node id, paths and control; unknown tools return an error to the model.
- `finish {text, reason}` — the node's last message and why it stopped
  (`message|budget|error`). There is no report schema: the host folds the record
  into a digest; the last message is the node's own account.

The response is the model message or tool-result JSON. Fatal host errors end the
run with `reason: error` in the record; tool errors are returned as JSON to the
model. When the live transcript outgrows its declared budget, the oldest tool
payloads are elided in place — the call and its result stay, the payload becomes
an accounted-for marker — while the host's record keeps every full result. Every JSON payload is at most 256 KiB, followed by one newline delimiter.
The loop serializes tool calls in model order and appends their results before
requesting another response. Incomplete provider responses and invalid/duplicate
tool calls fail the node. A spent budget is not a failure: when the step or
context budget is exhausted the node stops with `reason: budget` and EJA
continues with a new node. The system prompt states the node's turn and time
budget.

## Ownership and termination

The native node never executes shell commands, reads host configuration, opens
model connections, starts tool servers or creates child processes. Effects go
through `Model` and `ToolGateway` in the host. Their implementations are trusted
and must honor the supplied control and output bounds. `HostTools` routes granted
MCP tools through EJA's shared clients. No MCP configuration or connection is
created inside a node.

The host owns and reaps the worker process and joins its bounded IPC-reader
thread on every return path. Cancelling a node revokes its control; the HTTP
adapter observes cancellation while waiting for the provider. Host pipe closure
terminates the native loop. No idle session or process is retained.

The host then copies validated artifacts into the node's artifact directory,
marks files read-only, removes work, and publishes `node.json` by atomic rename.
The manifest includes the assignment, the host's digest, `context:
"context.jsonl"`, the frozen context size and `frozen: true`. The digest folds
the record the host observed: steps, tool calls, sealed artifacts, how the run
ended, and the node's last message. The context file is that record in full:
start messages, assistant turns, tool calls with arguments and results, and the
finish entry. A file the node names but never writes is not an artifact, and a
partial write stays inspectable. EJA's acceptance is a later log event and never
keeps the process alive.

Node ids are reserved by creating their directory once. Replays/restarts cannot
overwrite an existing node. An interrupted, unsealed directory stays reserved
for inspection; it is never automatically rerun. Continuation creates a new node.
Freezing is an application lifecycle guarantee, not an OS security sandbox or a
rollback of remote side effects. MCP servers are application resources, retained
across nodes and killed/reaped on orderly host shutdown (including their ordinary
process-group descendants on Unix). Cancelling a node cancels its pending MCP
request; it does not shut down the shared server or undo remote effects.
