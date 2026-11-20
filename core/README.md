# EJA Core

EJA 是一个连续的、有自我同一性的主体。它与人共同成长，借助 LLM 形成判断，持续关心某些事，并在授权范围内主动行动。它的价值在于让一个人能关心和影响的范围，超出此刻能够亲自关注的范围。

内核用一份日志维持一个主体的经历。一次模型调用只负责当下的判断。人的反馈、EJA 留下的关切、它实际做过的事和后果，都可以改变下一次判断。宿主决定哪些世界事件能被它看见，以及它获准怎样行动。

```text
观察 → 取当前证据 → 模型判断
          ↑           ├─ keep / release   更新或放下关切
          │           ├─ read / search    按 ID 回读或按语义检索旧证据
          │           ├─ act              宿主执行并写回执
          └───────────┘
                      rest 或步数用尽 → 交还宿主
```

## 接入一个应用

这是独立的 Rust crate，默认零 ML 运行时依赖。应用通过 `Eja::open(path)` 打开纯日志主体，或通过 `Eja::open_with_encoder(path, encoder)` 显式挂载语义编码器。宿主决定何时唤醒，使用 `notice` 写入人的原话、环境观察或时钟信号，然后调用 `turn(grants, steps, judge, perform)`。示例在 `examples/loop.rs`，只使用公开 API。

- **模型适配器**实现 `judge(&View) -> Result<Judgment, Box<dyn Error + Send + Sync>>`。每个判断都以 `basis` 指明促使本次判断的已记录事实 ID，并选出 6 种正交操作之一：
  - `keep`：`{"op":"keep","concern":null或已有ID,"text":"..."}` 新建或修订开放关切；开放关切跨回合自动保持，无需为维持存活重复提交。
  - `release`：`{"op":"release","concern":ID}` 放下开放关切。
  - `read`：`{"op":"read","fact":ID}` 按 ID 查阅任意历史事实，下一步在 `View.read` 中以 `Evidence`（原始 `Entry` 及其最新后续 `latest`）呈现；该步跳过自动向量召回。
  - `search`：`{"op":"search","cue":"..."}` 在已挂载编码器时按语义主动检索旧历史，下一步在 `View.related` 中呈现候选。
  - `act`：`{"op":"act","capability":"...","input":...}` 发起已获本轮授权的能力调用。
  - `rest`：`{"op":"rest"}` 结束本回合。
- **能力适配器**实现 `perform(Call) -> Outcome`。能力名与参数由应用定义；core 在单个 `IMMEDIATE` 事务内校验版本、依据、授权和行动幂等并提交判断后，才交出 `Call`。适配器负责校验参数、资源范围与实际权限，返回 `Observed`、`Failed` 或 `Unknown`。
- **内核边界与错误模型**。内核只守确定性不变量，不静默吞掉判断，也不替模型改写语义。`Error` 显式区分 `Conflict`（视图版本竞争）、`Invalid`（违反结构/引用/授权/幂等不变量）、`Storage` 与 `External`。需接异步运行器时可按 `view → decide → finish` 分步驱动。
- **历史与视图**。原始日志仅存不可变 `Entry { id, fact }`；检索视图使用独立的 `Evidence { entry, latest }` 与 `Match { evidence, similarity }`。每次 `View` 固定在同一 `revision`，呈现最多 8 项活跃关切和 8 条最近事实；单条事实 JSON 上限为 8192 字节，历史和关切均可通过 `history`、`concerns` 按游标翻页。
- **中断、竞争与回执**。同一视图版本仅能提交一个判断，竞争方收到 `Error::Conflict` 后重取视图再判。同一 `(basis, capability, input)` 的外部行动只允许发出一次，再次行动须基于新的触发证据（如新观察或回执）。重启只读日志，不会自动重试外部调用。`finish` 支持相同回执幂等重交，也允许 `Unknown` 回执在核实后被 `Observed` 或 `Failed` 结算一次；超大回执会截断并保留原始字节数和 SHA-256。

## 从全历史召回

`eja-core` 通过 `Encoder` trait 对接任意向量空间，与模型格式、语言和部署方式解耦：

- 未挂载编码器时（`Eja::open`），`View.related` 为 `None`（序列化省略），显式 `Choice::Search` 或 `Eja::search` 返回 `Error::Invalid`。
- 挂载编码器时（`Eja::open_with_encoder`），向量索引作为 `facts` 的单一衍生缓存（`memory_meta` / `memory_keys` / `memory_refs` / `memory_vectors`）按需增量补建；更换编码器签名时自动清空并重建衍生索引，不影响主体日志。
- 启用可选 feature `onnx` 后可使用 `OnnxEncoder::load(dir)` 显式加载包含 `model_quantized.onnx`、tokenizer 文件和 `encoder.json` 的本地模型目录；`OnnxEncoder` 支持廉价 `Clone` 以便跨主体或重开复用权重。可在 `core/` 下运行 `./scripts/install-memory-model.sh` 安装校验 SHA-256 的多语言 E5 权重。

- 人的原话、世界/时钟观察、关切文本、能力调用及回执文本均参与建索引；超过 384 字符的文本按 320 步长重叠切分，相同片段只编码一次并在结果中至多占 1 个槽位，避免重复样板淹没 top-4 召回。
- 默认以最近一条观察文本作为查询词；最新一步为 `Search { cue }` 时改用模型给出的 `cue`；最新一步为 `Read { fact }` 时跳过自动检索，让 `View.read` 居中呈现。最多 4 条旧历史候选进入 `View.related`，若旧事实后续已被更新或结算，其 `latest` 字段会一并带出。

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
