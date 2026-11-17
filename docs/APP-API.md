# The app API

One runtime, any number of frontends. A view talks to `eja serve` over HTTP and
renders what the runtime derives; it never recomputes interaction semantics.
`app/frontend` and `app/macos` are two views over this one contract.

All responses are JSON. An error is `{ "error": "..." }` with a 4xx/5xx status.

## GET /api/state

The view's snapshot. Query parameters:

- `busy=1` while the view has a turn in flight (the runtime also knows its own).
- `anchor=<event id>` while a view is in an encounter. The runtime filters
  `loops` and fills `spoken` for that exchange.
- `taskId=<id>`, `proposalId=<id>` — the encounter's source, so its own loop
  stays visible even after newer events.

Response:

```json
{
  "events": [{ "id", "at", "turn", "scope", "kind", "text", "closes", "data" }],
  "tasks": [{ "id", "kind", "title", "status", "workspace", "...": "..." }],
  "loops": [Loop],
  "presence": Message | null,
  "spoken": [Message],
  "lastEvent": 42,
  "health": { "model", "modelName", "state", "mcp", "journal", "journalError", "torn", "turnInFlight" }
}
```

`presence` is eja's own reply to the latest return, or `null` when it chose not
to speak or the user has since left. `spoken` is the encounter's exchange: what
was said since `anchor`, without eja's inner monologue (an eja message appears
only when the user was present in its turn). `lastEvent` is the bookmark a view
uses to open an encounter.

A `Loop` is one thing that still asks for the user:

```json
{ "key", "kind": "interrupted" | "decision" | "review", "eventId", "at", "title",
  "proposal": Proposal?, "task": Task? }
```

- `decision` carries `proposal`; answer it with `approvals` or `denials`.
- `review` and `interrupted` carry `task`; open the task.
- Order is fixed by the runtime: interrupted, then decisions, then reviews;
  newest first inside a rank. An answered proposal never appears again.

## POST /api/turn

```json
{ "text": "...", "focus": 7, "approvals": [], "denials": [], "gesture": "return" }
```

`gesture` is `return`, `leave` or `alone`, and stands alone (no text). A turn
with text opens/continues an encounter; `focus` names the task under discussion.
Response: `{ "ok": true, "reply": "..." }`, or `{ "ok": false, "error": "..." }`.

## GET /api/settings, POST /api/settings

The settings view is the connector list's projection: the built-in `workspace`
row is first, `builtin: true`, switchable and not deletable. Saving takes
providers, roles, non-builtin connectors, limits and workspaces; see
`src/host/settings.rs` for the exact shape. A save returns the new view.

## GET /api/raw

`address` is one documented address: `log:task:<id>`, `log:event:<id>`,
`record:<id>/aja.json`, `record:<id>/context.jsonl`.

## POST /api/provider/discover, POST /api/connectors/test

Provider discovery and connector probes used by settings. Bodies are the draft
provider/connector; both return a small result record.

## CORS

Local origins are accepted by default; additional frontends are named in
`--allow-origin`. The runtime serves no HTML: a frontend is a separate process.
