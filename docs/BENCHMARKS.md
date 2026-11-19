# Benchmarks

This file separates three kinds of evidence:

1. the current kernel measurements, reproduced from the working tree;
2. the historical, preselected model run that predates the current runtime;
3. the protocol required for a current model run, which has not been executed.

They are not interchangeable. Kernel latency does not measure task quality, and
an older model score does not describe EJA 0.14.0.

## Current kernel measurement

Measured on 2026-11-19 from the working tree based on EJA 0.14.0 commit
`fedac1b`, on macOS 27.2 arm64 with Rust 1.98.0, an optimized `release` build,
and no network dependency:

```sh
cargo run --locked --offline --release --example kernel_bench
```

The benchmark now initializes a valid state directory before appending its
fixture, reopens the journal, and asserts the expected event count. Each scale
row uses a fresh journal. `read` is one `inspect_journal` scan of newline-
committed events; `fold` is `State::fold` over the already parsed events; `HTTP
refresh` creates a cold `host::state::Cache`, reads and folds the journal, and
renders the API state. Iterations are 20, 10, 5 and 3 for 1k, 5k, 10k and 50k
events respectively. Times are milliseconds.

| events | read p50/p95 | fold p50/p95 | HTTP refresh p50/p95 | log bytes | peak RSS KiB |
|---:|---:|---:|---:|---:|---:|
| 1,000 | 0.221 / 0.281 | 0.032 / 0.060 | 0.884 / 1.005 | 102,465 | 10,224 |
| 5,000 | 1.027 / 1.145 | 0.164 / 0.300 | 4.338 / 4.872 | 529,468 | 24,192 |
| 10,000 | 2.052 / 2.087 | 0.317 / 0.437 | 8.941 / 9.788 | 1,064,471 | 41,872 |
| 50,000 | 10.883 / 10.954 | 1.994 / 2.080 | 49.066 / 52.735 | 5,494,474 | 212,896 |

Peak RSS is the process high-water mark after each row, so it is monotonic and
is not an incremental per-row allocation measurement.

Other measurements from the same run:

| workload | result |
|---|---|
| append + fsync, 100 events | p50 3.002 ms, p95 4.024 ms, 318.8 ops/s |
| append + fsync, 500 events | p50 3.971 ms, p95 4.028 ms, 285.6 ops/s |
| append + fsync, 2,000 events | p50 3.991 ms, p95 4.025 ms, 272.4 ops/s |
| empty turns | p50 6.990 ms, p95 8.014 ms |
| task turns | p50 22.970 ms, p95 24.006 ms, 20/20 done |
| 200 reads, 1,000 events | 731.5 ms |
| 200 reads, 10,000 events | 784.1 ms |
| 200 reads, 50,000 events | 785.2 ms |
| propose + confirm | 12.294 ms per turn over 100 turns |
| recover 100 interrupted tasks | 385.094 ms, 100 recovered |
| read a 16 KiB artifact | p50 0.122 ms, p95 0.154 ms |

These are single-machine observations, not performance guarantees. The history
table is useful for tracking regressions in the current implementation; it does
not establish a throughput ceiling for concurrent frontends.

## Historical model run

These results were recorded at `edbd448`, before the 0.11 structural changes,
and have not been rerun against 0.14.0.

One run, the then-current code, 23 hardest tasks of HarnessBench (the
`refusal-subset` tasks that scored zero before the structural fixes), model
`deepseek-v4-flash-0731` (the "DeepSeek-V4-Flash" row of the public
leaderboard):

| | score | zeros | full | >= 0.8 | requests/task | tokens/task |
|---|---:|---:|---:|---:|---:|---:|
| EJA-AJA, hardest 23 | 0.618 | 1/23 | 5/23 | 8/23 | 25.0 | 851K |

A second run after removing the unused log-search retriever scored 0.612 (3
zeros, 6 full, 9 at 0.8+), i.e. the same within sampling noise.

Cost split: EJA 7.0 requests (112K tokens, 13%); ajas 17.9 requests (738K
tokens, 87%); EJA looks 15.4 times per task.

Same-subset history: 0.248 (report boundary) -> 0.350 (silent refusals fixed)
-> 0.539 (transcript economics) -> 0.618.

Public leaderboard, same model family, full 274 tasks (not this subset):
Codex 68.34, Hermes 67.35, OpenClaw 66.78, OpenCode 66.24, seven-harness
average 65.76. Those numbers average over all tasks; this number is over the
hardest subset, so it is a floor-style reading, not an equal-set comparison.
The cost gap is the clear one: 1.9-3.6x the tokens per task.

## Current model protocol

A current model claim requires a fresh run with all of the following recorded:

- the exact EJA commit or working-tree patch digest, build profile and host OS;
- a fixed task manifest containing task ids and a content hash;
- provider, exact model id, request parameters and any routing headers that can
  change behavior;
- per-run turn, step, time and token budgets, plus retry policy;
- independent verification of every delivered artifact, separate from EJA's own
  acceptance decision;
- raw `log.jsonl`, AJA records and verifier output retained under a content hash.

Report at least:

- externally verified task success rate and complete/unfinished counts;
- **false acceptance rate**: EJA accepted the task but independent verification
  failed;
- false rejection count;
- total and per-task input/output tokens, requests split between EJA and AJAs,
  wall-clock time, and p50/p95 per-task cost;
- continuation, interruption, recovery and missing-record counts;
- task-level results, not only an aggregate, so a reviewer can recompute the
  rate.

Prioritize tasks that require at least one continuation, a host-observed
artifact, or an explicit rejection. Reusing the historical 23-task subset is
appropriate for continuity, but it must be labelled as preselected and must be
compared with a fixed baseline under the same model and budget.

**Current status: not executed.** This environment has no model credentials and
no permitted external-model network run. No current task-quality, token-cost or
false-acceptance number is claimed.
