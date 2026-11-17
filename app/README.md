# eja app

`app/macos` is the native SwiftUI frontend for the eja runtime. It has one
surface, **talk**: either the return screen of open loops or one active
encounter. There is no new-chat, session list, or client-side proposal state.

## Run

Start the runtime from the repository root:

```sh
cargo run -- serve --dir .eja --project .
```

Build and open the app:

```sh
app/macos/build.sh
open app/macos/dist/Eja.app
```

The app talks to the HTTP API at <http://127.0.0.1:8787>. The runtime and app
are separate processes; the app never embeds the runtime.

## Talk

The return screen and encounter are projections of the journal. The app owns
only the local encounter bookmark, not a second task store:

| Visible state | Source |
|---|---|
| decision | an open proposal |
| review | a waiting task, or an open group whose children are terminal |
| interrupted | a task left running after no turn is in flight |
| working | a running task while a turn is actually in flight |

`bye` clears the local encounter pointer and sends a structured `leave` gesture.
It does not close tasks, resolve proposals, or create a session. Presence and
open loops come from the runtime; eja's sentences come from the journal, not
from client copy.

Settings configure the harness only: providers, model bindings, connectors,
limits, and workspaces. They are stored by the runtime in
`<stateDir>/app-settings.json` with mode `0600` where available. An older
`app/settings.json` is read once and migrated; do not copy it into the
repository.
