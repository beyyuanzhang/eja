# AJAS protocol (v1)

AJAS is the plural of AJA: EJA owns 1:n AJA sessions. This protocol describes one
AJA session.

Two worker adapters exist:

- `PiWorker` (direct): one Pi session per assignment; the raw task is passed as
  the prompt with the configured system prompt, cwd and timeout. No JSON wrapper
  and no EJA acceptance text.
- `SessionWorker` (command fallback): one process per assignment, configured with
  `EJA_AJA_COMMAND` and run through `sh -c`; the JSON contract below.

## Command worker stdin

```json
{
  "task_id": 1,
  "objective": "写一个发布检查清单，必须包含 build、smoke test、upload",
  "issued_at": 1762699200
}
```

## stdout

Exactly one JSON object:

```json
{
  "status": "done",
  "summary": "清单已生成",
  "artifacts": ["checklist.md"],
  "evidence": ["包含 build、smoke test、upload"],
  "open_questions": []
}
```

`status` is `done`, `blocked`, or `failed`.

- `done`: the worker claims the assignment is complete.
- `blocked`: it needs a decision or missing input.
- `failed`: it could not complete it.

`objective` is the raw user task (`origin`), not EJA's expanded acceptance text.
EJA keeps its acceptance criteria internally; it does not send them to the worker.
`PiWorker` receives this objective directly, without a wrapper prompt.

The report is a claim. EJA reads artifacts and decides whether to accept it.

## Failure mapping

| Situation | EJA records |
|---|---|
| non-zero exit | `failed` report with stderr excerpt |
| invalid/empty stdout | `failed` report with stdout excerpt |
| valid report | report as given |

## Rules

- Do not write to EJA's `log.jsonl`.
- Do not read or assume EJA's identity.
- The protocol gives no EJA log path; this is a contract, not a sandbox.
- Keep state inside the assignment's workspace.
- One assignment per process; exit after reporting.
- The worker runs in its own process group. On exit or timeout EJA terminates
  the whole group; do not leave background daemons or detached writers.
- `issued_at` is UTC Unix seconds from EJA's single clock; timestamps are UTC.
- stdout is protocol only; write logs to stderr.
