# EJA Core

EJA 是一个连续的、有自我同一性的主体。它与人共同成长，借助 LLM 形成判断，持续关心某些事，并在授权范围内主动行动。它的价值在于让一个人能关心和影响的范围，超出此刻能够亲自关注的范围。

内核用一份日志维持一个主体的经历。一次模型调用只负责当下的判断。人的反馈、EJA 留下的关切、它实际做过的事和后果，都可以改变下一次判断。宿主决定哪些世界事件能被它看见，以及它获准怎样行动。

```text
快回路（清醒 turn）：
观察 → 纯只读视图（含 now / consolidated） → 模型返回 Step（可批量）
          ↑                                    ├─ keep / release  单事务原子更新关切
          │                                    ├─ act             先落盘后执行并写回执
          │                                    └─ read / search   按 ID 回读或语义检索（不写日志）
          └──────────────────────────────────── 有 read / search / act 时带反馈继续；否则自然收敛 Done

慢回路（睡眠 consolidate）：
未巩固日志（按 REPLAY_PAGE 顺序分页） + 上次 consolidated → 模型折叠 → 单事务 CAS 写入 Consolidated（自动入向量索引）
```

## 接入一个应用

这是独立的 Rust crate，默认零 ML 运行时依赖。应用通过 `Eja::open(path)` 打开纯日志主体，或通过 `Eja::open_with_encoder(path, encoder)` 显式挂载语义编码器。宿主决定何时唤醒，使用 `notice` 写入人的原话、环境观察或时钟信号，然后调用 `turn(grants, steps, judge, perform)`。示例在 `examples/loop.rs`，只使用公开 API。

- **物理时间与永久日志**：每条 `Entry` 均附带单调非降的物理时间戳 `at: Timestamp`（SQLite 存 Unix 秒，JSON 输出 RFC 3339 UTC；测试与仿真可通过 `with_clock` 注入时钟），`View.now` 始终呈现当前物理时间。永久日志仅记录 6 种扁平 `Fact`：`Noticed`、`Kept`、`Released`、`Consolidated`、`Acted`、`Returned`。纯查询（`read` / `search`）和回合结束不写日志。
- **快回路（清醒 `turn`）**：模型适配器实现 `judge(&View) -> Result<Step, Box<dyn Error + Send + Sync>>`，可自由对接原生 tool calling。每个 `Step` 可同时包含多组 `keep`、`release`、`read`、`search`、`act`：
  - `keep`：`Keep { concern: Option<Id>, text }` 新建或修订开放关切；开放关切跨回合自动保持。
  - `release`：`Id` 放下开放关切。
  - `read`：`Id` 按 ID 查阅任意历史事实，下一步在 `View.reads` 中返回 `Evidence { entry, latest }`。
  - `search`：`String` 在已挂载编码器时按语义检索全历史，下一步在 `View.searches` 中返回 `SearchHits`。
  - `act`：`Act { capability, input }` 发起已获本轮授权的能力调用。
  - 若本步不含 `read` / `search` / `act`（纯状态更新或空 `Step`），回合立即自然收敛并返回 `Stop::Done`。
- **慢回路（睡眠 `consolidate`）**：宿主在安静期调用 `consolidate(windows, distill)`（或按 `unconsolidated → commit_consolidation` 异步驱动）。内核自动以 `REPLAY_PAGE = 32` 按时间顺序分页回放未巩固的非巩固事实，与上一条 `Consolidated` 一起交给模型折叠，再在单事务 CAS 内写回 `Fact::Consolidated { through, text }`；该回路天然不暴露外部能力、不可跳过未读历史，并自动进入语义向量索引。
- **能力适配器**实现 `perform(Call) -> Outcome`。能力名与参数由应用定义；core 在单个 `IMMEDIATE` 事务内通过 `commit` 校验视图版本（CAS）、关切存活和授权并落盘后，才交出 `Call`。适配器负责校验参数、资源范围与实际权限，返回 `Observed`、`Failed` 或 `Unknown`。
- **内核边界与错误模型**。内核只守确定性不变量，不静默吞错，也不替模型改写语义。`Error` 显式区分 `Conflict`（视图版本竞争）、`Invalid`（违反结构/引用/授权不变量）、`Storage` 与 `External`。需接异步运行器时可按 `view → commit → finish` 配合 `evidence` / `search` 分步驱动。
- **历史与视图**。`Eja::view()` 是纯只读快照，固定在同一 `revision` 与当前 `now`，呈现最多 8 项活跃关切、最新一条 `consolidated` 长期记忆和 8 条最近事实，不触发写操作或隐式向量检索。单条事实 JSON 上限为 8192 字节，历史与关切均可通过 `history`、`concerns` 按游标翻页。
- **中断、竞争与回执**。同一视图版本仅能提交一个变更批次，竞争方收到 `Error::Conflict` 后重取视图再判。重启只读日志，不会自动重试外部调用。`finish` 支持相同回执幂等重交，也允许 `Unknown` 回执在核实后被 `Observed` 或 `Failed` 结算一次；超大回执会截断并保留原始字节数和 SHA-256。

## 从全历史召回

`eja-core` 通过 `Encoder` trait 对接任意向量空间，与模型格式、语言和部署方式解耦：

- 未挂载编码器时（`Eja::open`），`Eja::has_encoder()` 为 `false`，`Step.search` 与 `Eja::search` 返回 `Error::Invalid`。
- 挂载编码器时（`Eja::open_with_encoder`），向量索引作为 `facts` 的单一衍生缓存（`memory_meta` / `memory_keys` / `memory_refs` / `memory_vectors`）按需增量补建；更换编码器签名时自动清空并重建衍生索引，不影响主体日志。
- 启用可选 feature `onnx` 后可使用 `OnnxEncoder::load(dir)` 显式加载包含 `model_quantized.onnx`、tokenizer 文件和 `encoder.json` 的本地模型目录；`OnnxEncoder` 支持廉价 `Clone` 以便跨主体或重开复用权重。可在 `core/` 下运行 `./scripts/install-memory-model.sh` 安装校验 SHA-256 的多语言 E5 权重。

- 人的原话、世界/时钟观察、关切文本、慢巩固记忆、能力调用及回执文本均参与建索引；超过 384 字符的文本按 320 步长重叠切分，相同片段只编码一次并在结果中至多占 1 个槽位，避免重复样板淹没 top-4 召回。
- 语义召回按需触发（通过 `Step.search` 或 `Eja::search`），每次查询返回最多 4 条历史匹配；若命中的旧事实后续已被更新或结算，其 `latest` 字段会一并带出。

设计依据见 [Dense Passage Retrieval](https://aclanthology.org/2020.emnlp-main.550/)、[多语言 E5 模型说明](https://huggingface.co/intfloat/multilingual-e5-small) 和 [SQLite 向量近邻查询](https://alexgarcia.xyz/sqlite-vec/features/knn.html)。

## 验证

纯内核（零 ONNX 依赖）：

```sh
cargo test --locked --all-targets
cargo run --locked --example loop
```

启用本地 ONNX 权重验证语义召回：

```sh
./scripts/install-memory-model.sh
cargo test --locked --features onnx -- --include-ignored
cargo run --release --locked --features onnx --example semantic_recall -- 2000 500
```

### 用生成模型验证连续性

安装多语言权重后，指定兼容 chat completions 的模型网关并运行：

```sh
EJA_JUDGE_URL=http://127.0.0.1:3425/v1/chat/completions \
EJA_JUDGE_MODEL=auto/gemini-4-argon \
cargo run --locked --features onnx --example live_continuity
```

跨模型交接场景（同一主体日志在两个模型间切换并观察主动性）：

```sh
EJA_JUDGE_URL=http://127.0.0.1:3425/v1/chat/completions \
EJA_JUDGE_MODEL=auto/gemini-4-argon \
EJA_NEXT_JUDGE_MODEL=opencode-go/deepseek-v4.1-flash \
cargo run --locked --features onnx --example live_continuity -- --shared
```
