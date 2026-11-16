# eja app

A small local host for the eja journal, with a Svelte 5 + TypeScript + Vite
interface. The app has two views over one continuous life:

- **talk** is the conversation. It is not a session list or a new-chat client;
  the journal itself is the thread.
- **unfold** is the same journal seen as work: lanes, time, dependencies, and
  the evidence behind each node. It is read-only. Any action starts in talk.

The drawer in the lower-left is the only settings entry. It configures the
harness only: Providers, the eja/AJA model bindings, connectors, limits, and
the eja binary. A Provider owns an endpoint and credentials; eja and AJA only
choose a Provider and model from that list. Goals, commitments, and corrections
stay in talk as proposals, where they can be approved.

## Run

From the repository root:

```sh
cargo build
npm --prefix app/frontend install
npm --prefix app/frontend run build
node app/server.mjs --dir .eja --project .
```

Then open <http://127.0.0.1:8787>. If the model is already configured through
environment variables, it is used as a fallback. The binary is discovered from
`target/debug/eja`, `target/release/eja`, or `EJA_BIN`.

Do not open `app/public/index.html` directly. A `file://` page has no API host,
so the app shows a startup instruction instead of pretending to be connected.

For frontend work, run the host and Vite together:

```sh
node app/server.mjs --dir .eja --project .
npm --prefix app/frontend run dev
```

Open <http://127.0.0.1:5173>. Vite proxies `/api` to the host on port `8787`.
`npm --prefix app/frontend run check` runs the Svelte and TypeScript checks.
Production builds are written to `app/public`.

The Node host itself still has no runtime dependencies; npm is only required to
build or develop the interface.

Options:

```text
--dir DIR       eja state directory (default: .eja)
--project DIR   project the aja works in (default: cwd)
--port PORT     HTTP port (default: 8787)
--host HOST     bind address (default: 127.0.0.1)
--binary FILE   eja binary (default: auto-detect)
```

## One source of truth

Both views read `log.jsonl` through `GET /api/state`; the browser keeps no
second task state. Sending a message starts the real `eja turn` process. The
host streams nothing and writes no journal events itself.

`GET /api/raw?address=...` exposes the evidence behind a UI address:
`log:event:`, `log:task:`, `record:`, and `world:`. Reads are bounded and
confined to their roots.

Settings are stored in `app/settings.json` with POSIX mode `0600` where
available. The file is ignored by git; do not copy it into a repository.

## Why the two views are shaped this way

Parallel work is real, but parallel work is not the conversation. Talk names
the one thing in front of you and lets eja explain the rest in prose. Unfold is
there when you want to see the whole shape, without turning the relationship
into a control panel.
