# Historical benchmark — edbd448

These results were recorded before the 0.11 structural changes. They have not been rerun for 0.11.

One run, the then-current code, 23 hardest tasks of HarnessBench (the `refusal-subset`
tasks that scored zero before the structural fixes), model
`deepseek-v4-flash-0731` (the "DeepSeek-V4-Flash" row of the public
leaderboard):

| | score | zeros | full | >=0.8 | requests/task | tokens/task |
|---|---:|---:|---:|---:|---:|---:|
| EJA-AJA, hardest 23 | 0.618 | 1/23 | 5/23 | 8/23 | 25.0 | 851K |

A second run after removing the unused log-search retriever scored 0.612 (3
zeros, 6 full, 9 at 0.8+), i.e. the same within sampling noise.

Cost split: EJA 7.0 requests (112K tokens, 13%), nodes 17.9 requests (738K
tokens, 87%); EJA looks 15.4 times per task.

Same-subset history: 0.248 (report boundary) -> 0.350 (silent refusals fixed)
-> 0.539 (transcript economics) -> 0.618.

Public leaderboard, same model family, full 274 tasks (not this subset):
Codex 68.34, Hermes 67.35, OpenClaw 66.78, OpenCode 66.24, seven-harness
average 65.76. Those numbers average over all tasks; this number is over the
hardest subset, so it is a floor-style reading, not an equal-set comparison.
The cost gap is the clear one: 1.9-3.6x the tokens per task.
