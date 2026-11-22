# EJA

EJA 是一个连续的、有自我同一性的主体。仓库采用极简三层结构：

- `core/`（`eja-core`）：主体内核，负责 SQLite 主体日志、状态机、关切、回合 `turn` 以及可选语义召回（`onnx` + `sqlite-vec`）。详见 `core/README.md`。
- `host/`（`eja-host`）：Rust 驱动循环与统一能力层，由三块组成：`llm.rs` 基于 `anchor` 将只追加主体日志确定性投影到模型上下文（超限时推进 `anchor` 重置窗口，单回合内临时附着工具结果），`hands.rs` 与 `mcp.rs` 将 `say` / `wake` / `shell` 和热加载 MCP 连接器统一到同一工具注册与执行接口，`runtime.rs` 负责事件桥接与配置同步。
- `macos/`（`EjaClient` + `EjaApp`）：纯薄 macOS 原生 SwiftUI 交互壳（对话、关切与唤醒查看、LLM 与 MCP 设置、菜单栏常驻）。详见 `macos/README.md`。

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

默认数据目录为 `~/Library/Application Support/Eja/`：
- `subject.db`：`eja-core` 永久主体日志
- `config.json`：LLM 与 MCP 连接器配置
- `wakes.json`：定时唤醒记录
- `window.json`：当前上下文窗口起点 `anchor`
