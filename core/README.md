# eja Core

EJA is a continuous assistant that can build a lasting relationship with one person. It uses a language model to make decisions, remembers what it has experienced, keeps track of things that matter, and can take actions within the permissions it has been given. Its goal is to help people keep up with more than they can personally attend to at any one moment.

## How it works

A single event log holds the history of one EJA instance. Each model call handles the decision at hand; it does not define a new assistant or a new history. Feedback from the person, ongoing concerns, actions, and their outcomes all inform future decisions. The host application decides which events EJA can see and which actions it is allowed to take.

## Using the core in an application

`eja-core` is a small, self-contained Rust crate. It has no machine-learning, vector-search, scheduling, or user-interface dependencies. An application opens an identity with `eja::open(path)`, decides when to wake it and which actions to authorize, and uses the core's atomic operations to read and write its history. See `examples/loop.rs` for an offline example.

- **A permanent, time-ordered history:** Every `Entry` has a non-decreasing physical timestamp (`at: Timestamp`). SQLite stores Unix seconds; JSON uses RFC 3339 UTC. Tests and simulations can inject a clock with `with_clock`, and `now()` returns the current monotonic physical time. The log records six flat fact types: `Noticed`, `Said`, `Kept`, `Released`, `Acted`, and `Returned`.
- **Atomic operations and transactions:**
  - `transaction(|tx| ...)` opens an exclusive SQLite `IMMEDIATE` transaction. It can atomically write facts and key-value metadata (`get_meta` / `set_meta`), without imposing application-specific meaning on that metadata.
  - `notice(source, text)` records a person's words, a world event, or a clock signal.
  - `say(text)` records EJA's reply.
  - `keep(concern, text)` and `release(concern)` open and close ongoing concerns. A transaction can use `release_with_witness` to link a concern's closure to an existing fact.
  - `act(revision, capability, input)` checks the log revision using compare-and-swap, records the action, and then returns a `Call`. The host is responsible for authorizing the capability.
  - `finish(action, outcome)` records the result. Replaying the same receipt is idempotent; an `Unknown` result can only be finalized once. Each fact is limited to 64 KiB. Larger receipts are truncated while preserving their original byte count and SHA-256 hash.
- **Read-only queries:** `revision()`, `read(id)`, `evidence(id)`, `evidence_at(id, revision)`, `entries_after(after, through)`, `recent(limit)`, `concerns(revision)`, and `unfinished_actions()` do not write to the log.
- **Errors:** `Error` has three variants: `Conflict` (revision race), `Invalid` (a structural, reference, or state-machine invariant was violated), and `Storage`.

Full-history keyword and semantic search are provided by the separate `eja-memory` crate in `../memory`. It depends only on `eja-core`'s public read-only API.

During ordinary search, the host learns useful search cues from existing `Acted` and `Returned` facts and transaction metadata. Search changes cite the source facts and previous version; the core treats these writes as generic actions.

## Verify

```sh
cargo test -p eja-core --all-targets
cargo run -p eja-core --example loop
```

Long-running checks cover four generations of search updates across restarts. Offline replay also checks that degraded candidates are rejected and retrieval stays bounded as history grows. On the same log, evaluation compares an inherited version with the original query and records the first request that surfaces a held-out receipt, the number of model calls, and answer correctness.

```sh
cargo test -p eja-host search_replay_rsi_from_identity -- --nocapture
eja_EVAL_CONFIG=/path/to/config.json cargo test -p eja-host search_live_rsi_from_identity -- --ignored --nocapture
```
