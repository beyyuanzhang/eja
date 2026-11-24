# EJA

EJA 是一个连续的、有自我同一性的主体。仓库采用清晰分层结构：

- `core/`（`eja-core`）：最小闭合主体内核（零 ML/向量扩展依赖，零宿主调度策略），仅负责 SQLite 只追加事实日志、单调物理时间、关切与行动-回执状态机，以及供宿主共享原子事务的 `host_meta` 元数据槽。详见 `core/README.md`。
- `memory/`（`eja-memory`）：独立的可选全历史检索层，仅消费 `eja-core` 公开只读 API；默认支持无磁盘缓存的关键词召回，启用 `onnx` feature 后可在独立 `memory.db` 中维护可丢弃的 `sqlite-vec` 衍生语义索引。
- `host/`（`eja-host`）：Rust 驱动循环与统一能力层：`state.rs` 通过 `core` 原子事务统一管理回合游标、定时唤醒、窗口锚点与版本升级；`llm.rs` 基于锚点将日志确定性投影到模型上下文；`hands.rs` 与 `mcp.rs` 统一注册并执行内核原语与热加载能力层（内嵌常驻 `shell`、自由实现的 `exec` 本地插件，以及 `stdio` / `http` MCP 能力）；`http.rs` 统一封装 HTTP 传输与文本截断；`runtime.rs` 驱动判断回合与前端事件桥接。
- `macos/`（`EjaLauncher` + `EjaClient` + `EjaUI`）：纯薄 macOS 原生 SwiftUI 交互壳（不可变启动器、宿主通信、对话、模型与统一能力管理）。详见 `macos/README.md`。

默认支持本地关键词查找旧事实，可选语义编码器。

## 下载

从 [GitHub Releases](https://github.com/beyyuanzhang/eja/releases/latest) 下载最新的 `Eja-<version>.dmg`（macOS 14+，Apple Silicon）和 `SHA256SUMS.txt`，将 `Eja.app` 拖入 `/Applications` 即可。

首次打开若被 macOS 拦截，执行：

```sh
xattr -dr com.apple.quarantine /Applications/Eja.app
```

发布由 GitHub Actions 全自动完成：更新 `Cargo.toml` 的 `version` 并推送到 `main` 后，CI 会在所有检查通过时自动打 `v<version>` Tag 并发布新的 `.dmg` 与校验文件。

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

SwiftPM 开发运行、前端测试和 DMG 打包见 [macOS 构建说明](macos/README.md)。

内核目录默认为 `~/Library/Application Support/ai.eja.kernel/`。

- `subject.db`：`eja-core` 永久事实日志与宿主原子元数据槽，使用当前 `PRAGMA user_version` 格式
- `memory.db`：`eja-memory` 在以 `--features onnx` 构建并配置 `EJA_ONNX_MODEL_DIR` 时按需生成的衍生向量索引缓存（可随时删除重建）
- `config.json`：LLM Provider、密钥与宿主参数（Seatbelt 保护，前端仅见脱敏状态）
- `oauth.json`：HTTP 能力的 OAuth 2.1 Token 与刷新凭证（Seatbelt 保护，与 `capabilities.json` 分离）
- `host.lock`：单实例操作系统文件锁（确保同一数据目录仅运行一个主体实例）

用户数据目录默认为 `~/Library/Application Support/Eja/`。

- `capabilities.json`：统一能力列表（支持 `exec` 本地插件、`stdio` 与 `http` MCP，变更自动热加载）
- `ui/`：可由 EJA 直接修改的前端目录（默认原生 SwiftUI 源码，运行 `ui/apply.sh` 编译生成 `ui/libEjaUI.dylib` 后由不可变启动壳 `Eja.app` 动态加载热更新，不改动 `.app` 本体签名与系统权限；按住 `Option` 启动 `Eja.app` 可恢复默认安全模式）

前端与宿主协议见 `PROTOCOL.md`（同时随 `Eja.app/Contents/Resources/PROTOCOL.md` 只读分发）。
