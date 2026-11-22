# EJA macOS

## 结构

- `../core/`（`eja-core`）：不可动内核，负责主体账本、状态机与挂念。
- `../host/`（`eja-host`）：极简 Rust 驱动循环与统一能力层，直接调用 `eja-core` 日志原语驱动回合。
  - **确定性日志投影与模型适配**（`llm.rs`，基于 `anchor` 的只追加日志窗口，支持 OpenAI 兼容与 Anthropic 协议及连通测试）。
  - **统一能力面**（`hands.rs` + `mcp.rs`，将 `wake` / `shell` 与 `stdio` / `HTTP` MCP 连接器统一到同一工具定义、授权与执行契约，修改 `connectors.json` 自动热加载）。
- `Sources/EjaClient/` + `Sources/EjaApp/`：纯薄 macOS 原生交互壳（对话、关切与唤醒查看、LLM 与 MCP 设置、菜单栏常驻），零核心业务逻辑。

## 构建与运行

打包成完整 `.app`（推荐，支持系统通知与菜单栏常驻）：

```sh
./macos/scripts/build-app.sh
open macos/build/Eja.app
```

生成拖拽安装的 `.dmg` 安装包：

```sh
./macos/scripts/build-dmg.sh
```

或直接用 SwiftPM 开发运行与测试：

```sh
cargo test --workspace
cargo build --release -p eja-host
(cd macos && swift test)
(cd macos && swift run EjaApp)
```

内核目录默认为 `~/Library/Application Support/ai.eja.kernel/`。

- `subject.db`：`eja-core` 永久主体日志与定时唤醒计划（`PRAGMA user_version` 版本迁移）
- `config.json`：LLM Provider、密钥与宿主参数（Seatbelt 保护，前端仅见脱敏状态）
- `kernel_version`：内核版本记录（升级 `Eja.app` 后自动感知版本变更）
- `host.lock`：单实例操作系统文件锁（确保同一数据目录仅运行一个主体实例）
- `window.json`：当前上下文窗口起点 `anchor`

用户数据目录默认为 `~/Library/Application Support/Eja/`。

- `connectors.json`：MCP 连接器列表（变更自动热加载）
- `ui/`：可由 EJA 直接修改的前端目录（默认原生 SwiftUI 源码，运行 `ui/apply.sh` 编译并封装为完整 `ui/Eja.app` 后热切；启动 `Eja.app` 时按住 `Option` 可进入官方默认安全模式）
