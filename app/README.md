# eja app

A small local host for the eja journal, with a Svelte 5 + TypeScript + Vite
interface. The app has two views over one continuous life:

- **talk** is the conversation. It is not a session list or a new-chat client;
  the journal itself is the thread.
- **unfold** is the same journal seen as work: lanes, time, dependencies, and
  the evidence behind each node. It is read-only. Any action starts in talk.

The drawer in the lower-left is the only settings entry. It configures the
harness only: Providers, the eja/AJA model bindings, connectors, and limits. A
Provider owns an endpoint and credentials; eja and AJA only choose a Provider
and model from that list. Goals, commitments, and corrections stay in talk as
proposals, where they can be approved.

## Run

The app has one address: <http://127.0.0.1:4173>. The runtime and frontend
remain separate programs, but the runtime's port is internal transport, not a
second app entry.

Start the runtime from the repository root:

```sh
cargo run -- serve --dir .eja --project .
```

In another terminal, build and start the frontend:

```sh
npm --prefix app/frontend install
npm --prefix app/frontend run build
npm --prefix app/frontend run preview
```

Open <http://127.0.0.1:4173>.

Do not open `app/public/index.html` through `file://`; a static file has no HTTP
origin for the API.

For frontend editing, `npm --prefix app/frontend run dev` serves the same
<http://127.0.0.1:4173> address with hot reload. Stop the preview process first
so only one frontend entry exists.

For a frontend on another origin, build with its runtime address and allow that
origin when starting the runtime:

```sh
VITE_API_BASE=http://127.0.0.1:8787 npm --prefix app/frontend run build
eja serve --allow-origin https://frontend.example
```

`npm --prefix app/frontend run check` runs the Svelte and TypeScript checks.
Production builds are written to `app/public`.

Runtime options:

```text
--dir DIR       eja state directory (default: .eja)
--project DIR   project the aja works in (default: cwd)
--port PORT     HTTP port (default: 8787)
--host HOST     bind address (default: 127.0.0.1)
--allow-origin  additional frontend origin (repeatable or comma-separated)
```

## One source of truth

Both views read `log.jsonl` through `GET /api/state`; the browser keeps no
second task state. Sending a message runs a real turn inside the runtime. The
API streams nothing and writes no journal events of its own.

`GET /api/raw?address=...` exposes the evidence behind a UI address:
`log:event:`, `log:task:`, `record:`, and `world:`. Reads are bounded and
confined to their roots.

Settings are stored in `<stateDir>/app-settings.json` with POSIX mode `0600`
where available. An older `app/settings.json` is read once and migrated. The
current file is ignored by git; do not copy it into a repository.

The runtime binds to loopback by default. It has no authentication, so binding
with `--host 0.0.0.0` is only appropriate on a trusted network.

## Why the two views are shaped this way

Parallel work is real, but parallel work is not the conversation. Talk names
the one thing in front of you and lets eja explain the rest in prose. Unfold is
there when you want to see the whole shape, without turning the relationship
into a control panel.
