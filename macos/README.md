# EJA macOS

## 结构

- `../core/`（`eja-core`）：不可动内核，负责主体账本、状态机与挂念。
- `../host/`（`eja-host`）：极简 Rust 驱动循环与统一能力层，直接调用 `eja-core` 日志原语驱动回合。
  - **确定性日志投影与模型适配**（`llm.rs`，基于 `anchor` 的只追加日志窗口，支持 OpenAI 兼容与 Anthropic 协议及连通测试）。
  - **统一能力面**（`hands.rs` + `mcp.rs`，将 `wake` / `shell` 与 `stdio` / `HTTP` MCP 连接器统一到同一工具定义、授权与执行契约，修改 `config.json` 自动热加载）。
- `Sources/EjaClient/` + `Sources/EjaApp/`：纯薄 macOS 原生交互壳（对话、关切与唤醒查看、LLM 与 MCP 设置、菜单栏常驻），零核心业务逻辑。

## 构建与运行

打包成完整 `.app`（推荐，支持系统通知与菜单栏常驻）：

```sh
./macos/scripts/build-app.sh
open macos/build/Eja.app
```

或直接用 SwiftPM 开发运行与测试：

```sh
cargo test --workspace
cargo build --release -p eja-host
(cd macos && swift test)
(cd macos && swift run EjaApp)
```

数据默认保存在 `~/Library/Application Support/Eja/`：
- `core/subject.db`：`eja-core` 永久主体日志
- `core/wakes.json`：EJA 自设的定时唤醒
- `core/window.json`：当前上下文窗口起点 `anchor`
- `config.json`：LLM 与 MCP 连接器配置
