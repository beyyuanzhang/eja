# EJA Core

EJA 是一个连续的、有自我同一性的主体。它与人共同成长，借助 LLM 形成判断，持续关心某些事，并在授权范围内主动行动。它的价值在于让一个人能关心和影响的范围，超出此刻能够亲自关注的范围。

内核用一份日志维持一个主体的经历。一次模型调用只负责当下的判断。人的反馈、EJA 留下的关切、它实际做过的事和后果，都可以改变下一次判断。宿主决定哪些世界事件能被它看见，以及它获准怎样行动。

## 接入一个应用

`eja-core` 是最小闭合的独立 Rust crate，零 ML 与向量扩展依赖，零宿主调度或 UI 概念。应用通过 `Eja::open(path)` 打开主体，宿主自行决定何时唤醒、如何授权与调度，并通过原子原语读写日志。离线示例见 `examples/loop.rs`。

- **物理时间与永久日志**：每条 `Entry` 均附带单调非降的物理时间戳 `at: Timestamp`（SQLite 存 Unix 秒，JSON 输出 RFC 3339 UTC，测试与仿真可通过 `with_clock` 注入时钟），`now()` 返回当前单调物理时间。事实包括 `Noticed`、`Said`、`Kept`、`Released`、`Acted`、`Returned` 和 `Reflected`。
- **原子原语与组合事务**：
  - `transaction(|tx| ...)` 提供独占 `IMMEDIATE` SQLite 事务，允许宿主将事实写入与无业务语义的键值元数据槽（`get_meta` / `set_meta`）原子提交。
  - `notice(source, text)` 写入人的原话、世界事件或时钟信号。
  - `say(text)` 记录主体的文本回复。
  - `keep(concern, text)` / `release(concern)` 维护开放关切生命周期。
  - `act(revision, capability, input)` 在事务内校验日志版本（CAS）并先落盘，再交出 `Call`（能力授权由宿主负责）。
  - `finish(action, outcome)` 记录执行回执，支持相同回执幂等重交与 `Unknown` 单次终结，单条事实上限 64 KiB，超大回执截断并保留原始字节数与 SHA-256。
  - `record_reflection(reflection)` 将用户纠正、观察版本、快照、宿主实际召回并显示的事实 ID、模型结论、可选作用域 `when`、可选新线索与可选退役标记 `retire` 原子写进 `Reflected`。每条纠正最多写入一次。`observations_at(through)` 按日志版本返回各作用域当前活跃（未退役）的反思线索，`observation_at(through)` 返回最新一条。
- **严格只读查询**：`revision()`、`read(id)`、`evidence(id)`、`evidence_at(id, revision)`、`entries_after(after, through)`、`recent(limit)`、`concerns(revision)`、`unfinished_actions()` 均无任何写副作用。
- **内核错误模型**：`Error` 仅包含 `Conflict`（版本竞争）、`Invalid`（违反结构、引用或状态机不变量）与 `Storage`。

全历史关键词与语义召回位于独立 crate `../memory`（`eja-memory`），仅依赖 `eja-core` 公开只读 API。

## 验证

```sh
cargo test -p eja-core --all-targets
cargo run -p eja-core --example loop
```

使用指定模型在临时主体上实测两代反思链和留出案例。

```sh
EJA_EVAL_CONFIG=/path/to/config.json cargo test -p eja-host reflection_live_two_generations -- --ignored --nocapture
```

验证第二代线索是否实际影响后续判断。测试保留同一批反思，比较第二代线索生效和冻结在第一代时的留出答案，并计入缺失回执的案例。设置 `EJA_RSL_TEST_SEED` 可更换合成标签和凭证码。

```sh
EJA_EVAL_CONFIG=/path/to/config.json cargo test -p eja-host reflection_live_causal_inheritance -- --ignored --nocapture
```

验证多主题交替演进不会互相覆盖，并直接迁移到常规用户对话回合（`send`）且不污染无关话题。

```sh
EJA_EVAL_CONFIG=/path/to/config.json cargo test -p eja-host reflection_live_multi_topic_turn_transfer -- --ignored --nocapture
```

纯 `send` 回合端到端验证自主最小 RSI 闭环（零外部 `reflect` 指令）：自主建线索、两代递归升级、1 步直出对照、零字面重叠同义改写命中以及自主退役清零。

```sh
EJA_EVAL_CONFIG=/path/to/config.json cargo test -p eja-host reflection_live_autonomous_full_rsi -- --ignored --nocapture
```

