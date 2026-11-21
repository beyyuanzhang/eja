# EJA macOS Origin

基于 `core/`（`eja-core`，一行不改）的三层极简原点：

## 结构

- `../core/`（`eja-core`）：不可动内核，负责主体账本、状态机、挂念与记忆巩固。
- `../host/`（`eja-host`）：极简 Rust 运行时与能力层，直接驱动 `eja.turn()` / `eja.consolidate()`，仅包含两个核心能力模块：
  - **LLM Provider 管理与模型分配**（支持多 Provider、OpenAI 兼容与 Anthropic 协议、主思考/记忆整理角色分配与连通测试）。
  - **统一标准 MCP 连接器**（`stdio` + `HTTP`，统一接入外部 MCP Server 与 EJA 自建 MCP，修改 `config.json` 自动热加载），外加 3 个最小内置原语（`say` / `wake` / `shell`）。
- `Sources/EjaClient/` + `Sources/EjaApp/`：纯薄 macOS 原生交互壳（对话、记忆查看、LLM 与 MCP 设置、菜单栏常驻），零核心业务逻辑。

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
- `subject.db`：`eja-core` 永久主体日志
- `config.json`：LLM 与 MCP 连接器配置
- `wakes.json`：EJA 自设的定时唤醒
