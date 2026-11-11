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
  "assignment": {"task_id": 1, "objective": "write the checklist", "issued_at": 1762699200},
  "limits": {"timeout_ms": 300000, "steps": 16, "context_bytes": 262144, "response_bytes": 65536}
}
```

`objective` is the specific node request. The EJA log, original user history,
acceptance text, credentials and connector configuration are not sent.

## Requests

The node sends one request and waits for the host response:

- `model {messages}` — host calls its configured model with the run's tool definitions.
- `tool {name, arguments}` — host checks the snapshotted grant and binds the real
  node id, paths and control; unknown tools return an error to the model.
- `finish {report}` — final `done|blocked|failed` claim with summary, relative
  work artifacts, evidence and open questions.

The response is the model message or tool-result JSON. Fatal host errors close
the run and produce a failed report; tool errors are returned as JSON to the
model. Every JSON payload is at most 256 KiB, followed by one newline delimiter.
The loop serializes tool calls in model order and appends their results before
requesting another response. Malformed reports, incomplete provider responses,
invalid/duplicate tool calls and exhausted limits fail the node.

## Ownership and termination

The native node never executes shell commands, reads host configuration, opens
model connections, starts tool servers or creates child processes. Effects go
through `Model` and `ToolGateway` in the host. Their implementations are trusted
and must honor the supplied control and output bounds. External MCP integration
is a host adapter, not an implicit feature of the node.

The host owns and reaps the worker process and joins its bounded IPC-reader
thread on every return path. Cancelling a node revokes its control; the HTTP
adapter observes cancellation while waiting for the provider. Host pipe closure
terminates the native loop. No idle session or process is retained.

The host then copies validated artifacts into the node's artifact directory,
marks files read-only, removes work, and publishes `node.json` by atomic rename.
The manifest includes the assignment, report and `frozen: true`. Invalid artifact
claims produce a failed report; valid partial writes remain inspectable. EJA's
acceptance is a later log event and never keeps the process alive.

Node ids are reserved by creating their directory once. Replays/restarts cannot
overwrite an existing node. An interrupted, unsealed directory stays reserved
for inspection; it is never automatically rerun. Continuation creates a new node.
Freezing is an application lifecycle guarantee, not an OS security sandbox or a
rollback of remote side effects. A future process-executing connector must own
and settle its execution domain before its call returns.
