# EJA Core

EJA 是一个连续的、有自我同一性的主体。它与人共同成长，借助 LLM 形成判断，持续关心某些事，并在授权范围内主动行动。它的价值在于让一个人能关心和影响的范围，超出此刻能够亲自关注的范围。

内核用一份日志维持一个主体的经历。一次模型调用只负责当下的判断。人的反馈、EJA 留下的关切、它实际做过的事和后果，都可以改变下一次判断。宿主决定哪些世界事件能被它看见，以及它获准怎样行动。

## 接入一个应用

这是独立的 Rust crate，默认零 ML 运行时依赖。应用通过 `Eja::open(path)` 打开纯日志主体，或通过 `Eja::open_with_encoder(path, encoder)` 显式挂载语义编码器。宿主决定何时唤醒，使用 `notice` 写入人的原话、环境观察或时钟信号，直接通过原子日志原语推进状态并记录回复。示例在 `examples/loop.rs`，只使用公开 API。

- **物理时间与永久日志**：每条 `Entry` 均附带单调非降的物理时间戳 `at: Timestamp`（SQLite 存 Unix 秒，JSON 输出 RFC 3339 UTC，测试与仿真可通过 `with_clock` 注入时钟），`now()` 返回当前单调物理时间。永久日志记录 6 种扁平 `Fact`：`Noticed`、`Said`、`Kept`、`Released`、`Acted`、`Returned`。纯查询（`read` / `evidence` / `search` / `entries_after` / `recent` / `concerns`）不写日志。
- **原子原语**：
  - `notice(source, text)` 写入人的原话、世界事件或时钟信号。
  - `say(text)` 直接记录主体的文本回复。
  - `keep(concern, text)` 新建或修订开放关切并返回落盘后的 `Entry`。
  - `release(concern)` 放下开放关切并返回落盘后的 `Entry`。
  - `act(revision, capability, input, grants)` 在 `IMMEDIATE` 事务内校验版本（CAS）与授权并先落盘，再交出 `Call`。
  - `finish(action, outcome)` 记录执行回执并返回落盘后的 `Entry`，支持相同回执幂等重交与 `Unknown` 单次结算，单条事实上限 64 KiB，超大回执截断并保留原始字节数与 SHA-256。
  - `read(id)` / `evidence(id)` / `entries_after(after, through)` / `recent(limit)` / `concerns(revision)` / `search(cue)` 提供单次 SQLite 查询的完整只读访问。
- **内核边界与错误模型**：内核只守确定性不变量，不静默吞错。`Error` 显式区分 `Conflict`（版本竞争）、`Invalid`（违反结构、引用或授权不变量）、`Storage` 与 `External`。

## 从全历史召回

`eja-core` 通过 `Encoder` trait 对接任意向量空间，与模型格式、语言和部署方式解耦：

- 未挂载编码器时（`Eja::open`），`Eja::search` 使用本地关键词匹配，支持中英文标点分词与部分命中按命中率排序。
- 挂载编码器时（`Eja::open_with_encoder`），向量索引作为 `facts` 的单一衍生缓存（`memory_meta` / `memory_keys` / `memory_refs` / `memory_vectors`）按需增量补建；更换编码器签名时自动清空并重建衍生索引，不影响主体日志。
- 启用可选 feature `onnx` 后可使用 `OnnxEncoder::load(dir)` 显式加载包含 `model_quantized.onnx`、tokenizer 文件和 `encoder.json` 的本地模型目录；`OnnxEncoder` 支持廉价 `Clone` 以便跨主体或重开复用权重。可在 `core/` 下运行 `./scripts/install-memory-model.sh` 安装校验 SHA-256 的多语言 E5 权重。

- 人的原话、主体回复、世界/时钟观察、关切文本、能力调用及回执文本均参与建索引；超过 384 字符的文本按 320 步长重叠切分，相同片段只编码一次并在结果中至多占 1 个槽位，避免重复样板淹没 top-10 召回。
- 历史召回按需触发（通过 `Eja::search`），每次查询返回最多 10 条历史匹配；若命中的旧事实后续已被更新或结算，其 `latest` 字段会一并带出。

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

