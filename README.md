# eja

**EJA is a personal AI assistant for your Mac that remembers what matters and helps you follow through.**

It can remember useful details across conversations, keep track of things you care about, and remind you when something needs attention. You choose which tools it can use, and its history stays on your Mac.

EJA is designed to feel like one ongoing assistant, rather than a series of unrelated chat sessions. Under the hood, it uses an append-only history to carry context across conversations and keep a record of actions and their outcomes.

## Install

Download `eja-<version>.dmg` from [Releases](https://github.com/beyyuanzhang/eja/releases/latest) (macOS 14+, Apple Silicon), then drag `eja.app` into `/Applications`.

If macOS blocks the app the first time you open it:

```sh
xattr -dr com.apple.quarantine /Applications/eja.app
```

When an update is available, the app shows a small notice in the upper-right corner. You can verify and install it locally, then restart. Hold `Option` while launching to enter the factory safe mode.

## Where your data lives

The app (`eja.app`) is read-only and replaced as a whole during updates. Its data lives in two locations:

- `~/Library/Application Support/ai.eja.kernel/`: kernel data, including the `subject.db` event log, `config.json`, `oauth.json`, and the `memory.db` vector cache. macOS Seatbelt isolates this directory.
- `~/Library/Application Support/eja/`: shared space for you and EJA. It contains `capabilities.json` (Shell, local `exec` plugins, and `stdio`/`http` MCP) and `ui/` (native SwiftUI source that EJA can modify). App updates sync the underlying client and rebuild `ui/` without overwriting the interface you have evolved.

## Repository layout

- `core/`: append-only event log, monotonic time, and action receipts ([core/README.md](core/README.md))
- `memory/`: read-only history search using a local embedding model and keywords
- `host/`: Rust runtime, model-turn orchestration, scheduled wakeups, and capability sandbox
- `macos/`: native SwiftUI shell, dynamic launcher, and built-in updater ([macos/README.md](macos/README.md))
- `PROTOCOL.md`: stdio JSON protocol between the UI and `eja-host`

## Build from source

```sh
./macos/scripts/build-app.sh
open macos/build/eja.app
```

Run checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```
