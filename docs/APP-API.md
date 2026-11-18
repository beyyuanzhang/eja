# The app API

One runtime, any number of frontends. A view talks to `eja serve` over HTTP and
renders what the runtime derives; it never recomputes interaction semantics.
`app/macos` is the native view over this contract.

All responses are JSON. An error is `{ "error": "..." }` with a 4xx/5xx status.

## GET /api/state

The view's snapshot. Query parameters:

- `busy=1` while the view has a turn in flight (the runtime also knows its own).
- `anchor=<event id>` while a view is in an encounter. The runtime filters
  `loops` and fills `spoken` for that exchange.
- `taskId=<id>`, `proposalId=<id>` — the encounter's source, so its own loop
  stays visible even after newer events.
- `after=<event id>` returns only events newer than that committed id. The
  runtime keeps a rebuildable derived cache, so polling does not fold the whole
  journal when the file is unchanged.

Response:

```json
{
  "events": [{ "id", "at", "turn", "scope", "kind", "text", "closes", "data" }],
  "tasks": [{ "id", "kind", "title", "status", "workspace", "...": "..." }],
  "loops": [Loop],
  "presence": Message | null,
  "spoken": [Message],
  "lastEvent": 42,
  "health": { "model", "modelName", "state", "tools", "journal", "journalError",
              "torn", "turnInFlight", "capabilityErrors" }
}
```

`presence` is eja's own reply to the latest return, or `null` when it chose not
to speak or the user has since left. A reply may be silent text plus optional
`choices`; choices are shortcuts, never a required answer. `spoken` is the
encounter's exchange: what was said since `anchor`, without eja's inner
monologue (an eja message appears only when the user was present in its turn).
`lastEvent` is the bookmark a view uses to open an encounter and the cursor for
the next `after` request. A frontend merges returned events with the events it
already holds. Tasks, loops and presence remain complete derived views.

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
{
  "text": "...",
  "focus": 7,
  "approvals": [],
  "denials": [],
  "gesture": "return"
}
```

`gesture` is `return`, `leave` or `alone`, and stands alone (no text). A turn
with text opens/continues an encounter; `focus` names the task under discussion.
Text remains a valid answer to any question, including a free-form answer such
as "the second one".

A button answer uses the question's recorded option instead:

```json
{ "selection": { "question": 12, "option": 0 } }
```

`selection` answers one choice question recorded in eja's `data.choices`; it
stands alone against text, approvals, denials and gestures. The runtime resolves
`option` against the recorded question, so a client cannot substitute its own
value. The answer becomes an ordinary user event carrying the exact value.
Response: `{ "ok": true, "reply": "..." }`, or `{ "ok": false, "error": "..." }`.

## GET /api/settings, POST /api/settings

The settings view contains providers, role bindings, a flat array of capability
cards, limits, workspaces and runtime paths. Every card has an id, title,
description, kind, source, enabled flag and JSON config. Saving accepts a map
keyed by capability id, each containing `{ "enabled", "config" }`. The MCP card
owns its connector list. Write-only provider and connector secrets are
preserved without being returned. A save returns the new view.

## GET /api/capabilities

Returns the same capability cards without provider or runtime settings. Active
managed plugins add `plugin:NAME` cards through the same catalog.

## GET /api/raw

`address` is one documented address: `log:task:<id>`, `log:event:<id>`,
`record:<id>/aja.json`, `record:<id>/context.jsonl`.

## POST /api/provider/discover, POST /api/capabilities/{id}/probe

Provider discovery and capability probes used by settings. The `mcp` card
accepts a draft connector envelope and returns its discovered tools. Built-in
cards without a host-side probe return 404.

## CORS

Local origins are accepted by default; additional frontends are named in
`--allow-origin`. The runtime serves no HTML: a frontend is a separate process.
