# EJA Core

EJA 是一个连续的、有自我同一性的主体。它与人共同成长，借助 LLM 形成判断，持续关心某些事，并在授权范围内主动行动。它的价值在于让一个人能关心和影响的范围，超出此刻能够亲自关注的范围。

内核用一份日志维持一个主体的经历。一次模型调用只负责当下的判断。人的反馈、EJA 留下的关切、它实际做过的事和后果，都可以改变下一次判断。宿主决定哪些世界事件能被它看见，以及它获准怎样行动。

## 接入一个应用

`eja-core` 是最小闭合的独立 Rust crate，零 ML 与向量扩展依赖，零宿主调度或 UI 概念。应用通过 `Eja::open(path)` 打开主体，宿主自行决定何时唤醒、如何授权与调度，并通过原子原语读写日志。离线示例见 `examples/loop.rs`。

- **物理时间与永久日志**：每条 `Entry` 均附带单调非降的物理时间戳 `at: Timestamp`（SQLite 存 Unix 秒，JSON 输出 RFC 3339 UTC，测试与仿真可通过 `with_clock` 注入时钟），`now()` 返回当前单调物理时间。永久日志只记录 6 种纯粹的扁平 `Fact`：`Noticed`、`Said`、`Kept`、`Released`、`Acted`、`Returned`。
- **原子原语与组合事务**：
  - `transaction(|tx| ...)` 提供独占 `IMMEDIATE` SQLite 事务，允许宿主将事实写入与无业务语义的键值元数据槽（`get_meta` / `set_meta`）原子提交。
  - `notice(source, text)` 写入人的原话、世界事件或时钟信号。
  - `say(text)` 记录主体的文本回复。
  - `keep(concern, text)` / `release(concern)` 维护开放关切生命周期。事务可用 `release_with_witness` 在关闭时关联一条已有事实。
  - `act(revision, capability, input)` 在事务内校验日志版本（CAS）并先落盘，再交出 `Call`（能力授权由宿主负责）。
  - `finish(action, outcome)` 记录执行回执，支持相同回执幂等重交与 `Unknown` 单次终结，单条事实上限 64 KiB，超大回执截断并保留原始字节数与 SHA-256。
- **严格只读查询**：`revision()`、`read(id)`、`evidence(id)`、`evidence_at(id, revision)`、`entries_after(after, through)`、`recent(limit)`、`concerns(revision)`、`unfinished_actions()` 均无任何写副作用。
- **内核错误模型**：`Error` 仅包含 `Conflict`（版本竞争）、`Invalid`（违反结构、引用或状态机不变量）与 `Storage`。

全历史关键词与语义召回位于独立 crate `../memory`（`eja-memory`），仅依赖 `eja-core` 公开只读 API。

宿主在普通检索回合中用已有的 `Acted`、`Returned` 与事务元数据学习检索线索。变更记录引用原始事实与前一版本。内核按通用动作处理这类写入。

## 验证

```sh
cargo test -p eja-core --all-targets
cargo run -p eja-core --example loop
```

长程验证覆盖四代检索更新和逐代重启。离线回放还验证了退化候选的拒绝与历史增长后的召回上限。实测在同一日志上对比继承版本与原查询，记录留出回执首次出现的请求、模型调用次数和答案正确性。

```sh
cargo test -p eja-host search_replay_rsi_from_identity -- --nocapture
EJA_RECALL_LEARNING=1 EJA_EVAL_CONFIG=/path/to/config.json cargo test -p eja-host search_live_rsi_from_identity -- --ignored --nocapture
```

