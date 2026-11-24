# EJA

EJA 是一个连续的、有自我同一性的主体。仓库采用清晰分层结构：

- `core/`（`eja-core`）：最小闭合主体内核（零 ML/向量扩展依赖，零宿主调度策略），仅负责 SQLite 只追加事实日志、单调物理时间、关切与行动-回执状态机，以及供宿主共享原子事务的 `host_meta` 元数据槽。详见 `core/README.md`。
- `memory/`（`eja-memory`）：独立的可选全历史检索层，仅消费 `eja-core` 公开只读 API；默认支持无磁盘缓存的关键词召回，启用 `onnx` feature 后可在独立 `memory.db` 中维护可丢弃的 `sqlite-vec` 衍生语义索引。
- `host/`（`eja-host`）：Rust 驱动循环与统一能力层：`state.rs` 通过 `core` 原子事务统一管理回合游标、定时唤醒、窗口锚点与版本升级；`llm.rs` 基于锚点将日志确定性投影到模型上下文；`hands.rs` 与 `mcp.rs` 统一注册并执行内核原语与热加载能力层（内嵌常驻 `shell`、自由实现的 `exec` 本地插件，以及 `stdio` / `http` MCP 能力）；`http.rs` 统一封装 HTTP 传输与文本截断；`runtime.rs` 驱动判断回合与前端事件桥接。
- `macos/`（`EjaClient` + `EjaApp`）：纯薄 macOS 原生 SwiftUI 交互壳（对话、模型与统一能力管理）。详见 `macos/README.md`。

默认支持本地关键词查找旧事实，可选语义编码器。

## 构建与验证

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

构建并运行 macOS 应用：

```sh
./macos/scripts/build-app.sh
open macos/build/Eja.app
```

或分步开发测试：

```sh
cargo build -p eja-host
(cd macos && swift test)
(cd macos && swift run EjaApp)
```

内核目录默认为 `~/Library/Application Support/ai.eja.kernel/`。

- `subject.db`：`eja-core` 永久事实日志与宿主原子元数据槽（回合游标、定时唤醒、上下文窗口锚点与内核版本，`PRAGMA user_version` 版本迁移）
- `memory.db`：`eja-memory` 在启用语义编码器时按需生成的衍生向量索引缓存（可随时删除重建）
- `config.json`：LLM Provider、密钥与宿主参数（Seatbelt 保护，前端仅见脱敏状态）
- `host.lock`：单实例操作系统文件锁（确保同一数据目录仅运行一个主体实例）

用户数据目录默认为 `~/Library/Application Support/Eja/`。

- `capabilities.json`：统一能力列表（支持 `exec` 本地插件、`stdio` 与 `http` MCP，变更自动热加载）
- `ui/`：可由 EJA 直接修改的前端目录（默认原生 SwiftUI 源码，运行 `ui/apply.sh` 编译生成 `ui/EjaUI` 后由 `Eja.app` 原地热更新；按住 `Option` 启动 `Eja.app` 可恢复默认安全模式）

前端与宿主协议见 `PROTOCOL.md`（同时随 `Eja.app/Contents/Resources/PROTOCOL.md` 只读分发）。
