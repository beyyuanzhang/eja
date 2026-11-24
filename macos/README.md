# EJA macOS

## 结构

- `../core/`（`eja-core`）：不可动内核，负责主体账本、状态机与挂念。
- `../host/`（`eja-host`）：极简 Rust 驱动循环与统一能力层，直接调用 `eja-core` 日志原语驱动回合。
  - **确定性日志投影与模型适配**（`llm.rs`，基于 `anchor` 的只追加日志窗口，支持 OpenAI 兼容与 Anthropic 协议及连通测试）。
  - **统一能力面**（`hands.rs` + `mcp.rs`，将内嵌常驻 `shell`、自由实现的 `exec` 本地插件与 `stdio` / `http` MCP 统一到同一能力注册、授权与执行契约，修改 `capabilities.json` 自动热加载）。
- `Sources/EjaClient/` + `Sources/EjaApp/`：纯薄 macOS 原生交互壳（对话、模型与能力管理），零核心业务逻辑。

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

- `subject.db`：`eja-core` 永久事实日志与宿主原子元数据槽（回合游标、定时唤醒、上下文窗口锚点与内核版本，`PRAGMA user_version` 版本迁移）
- `memory.db`：`eja-memory` 在启用语义编码器时按需生成的衍生向量索引缓存（可随时删除重建）
- `config.json`：LLM Provider、密钥与宿主参数（Seatbelt 保护，前端仅见脱敏状态）
- `host.lock`：单实例操作系统文件锁（确保同一数据目录仅运行一个主体实例）

用户数据目录默认为 `~/Library/Application Support/Eja/`。

- `capabilities.json`：统一能力列表（变更自动热加载）
- `ui/`：可由 EJA 直接修改的前端目录（默认原生 SwiftUI 源码，运行 `ui/apply.sh` 编译生成 `ui/libEjaUI.dylib` 后由不可变启动壳 `Eja.app` 动态加载热更新，不改动 `.app` 本体签名与系统权限；启动 `Eja.app` 时按住 `Option` 可恢复官方默认安全模式）
