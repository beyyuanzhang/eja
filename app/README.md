# eja app

`app/macos` is the native SwiftUI frontend for the eja runtime. It has one
surface, **talk**: either the return screen of open loops or one active
encounter. There is no new-chat, session list, or client-side proposal state.

## Run

Build the app and its bundled runtime:

```sh
app/macos/build.sh
open app/macos/dist/Eja.app
```

Drag `Eja.app` into Applications to install it. A normal launch needs no
terminal: the app starts the bundled `eja` runtime at
<http://127.0.0.1:8787> when nothing is listening there. If another runtime
already answers at the configured address, the app attaches to it and leaves it
alone.

Closing the window hides it; Eja remains in the menu bar and the runtime keeps
running. Quitting Eja stops the runtime only when this app started it. The
runtime is still a separate process and still serves the same HTTP API, so other
frontends are unaffected.

The app keeps its state under `~/Library/Application Support/Eja/state`, uses
`~/Library/Application Support/Eja/workspace` as the default project, and writes
runtime output to `~/Library/Logs/Eja/runtime.log`. The project directory can be
changed in Settings → 运行 → 本机实例.

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

Settings configure providers, model bindings, capability cards, limits and
workspaces. They are stored by the runtime in
`<stateDir>/app-settings.json` with mode `0600` where available. This is the
only settings file. Built-in and plugin-provided cards use the same toggle and
config shape. The same file drives the CLI and HTTP runtime.
