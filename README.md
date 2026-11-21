# EJA

EJA 是一个连续的、有自我同一性的主体。仓库采用极简三层结构：

- `core/`（`eja-core`）：主体内核，负责 SQLite 主体日志、状态机、关切、快回路 `turn`、慢回路 `consolidate` 以及可选语义召回（`onnx` + `sqlite-vec`）。详见 `core/README.md`。
- `host/`（`eja-host`）：Rust 运行时与能力层，直接驱动 `eja-core`，提供多 LLM Provider 管理（OpenAI 兼容 / Anthropic）、统一标准 MCP 连接器（`stdio` + `HTTP` 热加载）以及 `say` / `wake` / `shell` 最小内置原语。
- `macos/`（`EjaClient` + `EjaApp`）：纯薄 macOS 原生 SwiftUI 交互壳（对话、记忆查看、LLM 与 MCP 设置、菜单栏常驻）。详见 `macos/README.md`。

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
