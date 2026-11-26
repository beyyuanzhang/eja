# eja

住在你 Mac 上的单一连续主体。不是一堆用完即弃的对话窗口，而是一个共享同一条时间线、能记住发生过的事、能定时醒来做事、也能自己改写界面的本地应用。

## 安装

从 [Releases](https://github.com/beyyuanzhang/eja/releases/latest) 下载 `eja-<version>.dmg`（macOS 14+，Apple Silicon），把 `eja.app` 拖进 `/Applications`。

首次打开如果被 macOS 拦截：

```sh
xattr -dr com.apple.quarantine /Applications/eja.app
```

有新版本时，应用会在右上角给出轻量提示，可直接在本地校验并更新重启；按住 `Option` 启动可进入出厂安全模式。

## 数据放在哪里

应用本身（`eja.app`）只读、整体替换，数据分两处存放：

- `~/Library/Application Support/ai.eja.kernel/`：内核数据（`subject.db` 事实日志、`config.json`、`oauth.json`、`memory.db` 向量缓存），由 macOS Seatbelt 隔离保护。
- `~/Library/Application Support/eja/`：用户与 eja 共同拥有的空间——`capabilities.json`（Shell、本地 `exec` 插件、`stdio`/`http` MCP）与 `ui/`（eja 可自行修改的原生 SwiftUI 源码）。应用更新时会自动同步底层客户端并重编 `ui/`，不覆盖用户演化出的界面。

## 仓库结构

- `core/`：只追加事实日志、单调时间与行动回执内核（见 [core/README.md](core/README.md)）
- `memory/`：基于本地向量模型与关键词的只读历史检索
- `host/`：Rust 运行宿主、模型回合驱动、守望唤醒与能力沙箱
- `macos/`：原生 SwiftUI 外壳、动态加载启动器与内置更新（见 [macos/README.md](macos/README.md)）
- `PROTOCOL.md`：前端与 `eja-host` 的 stdio JSON 协议

## 从源码构建

```sh
./macos/scripts/build-app.sh
open macos/build/eja.app
```

运行校验：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```
