# eja

**eja: an identity.**

A single, continuing AI identity, carried across conversations.

## 中文

**eja：一个身份。**

同一个 AI 主体，跨越一次次对话，持续存在。

## Install / 安装

Download `eja-<version>.dmg` from [Releases](https://github.com/beyyuanzhang/eja/releases/latest) (macOS 14+, Apple Silicon), then drag `eja.app` into `/Applications`.

从 [Releases](https://github.com/beyyuanzhang/eja/releases/latest) 下载 `eja-<version>.dmg`（macOS 14+，Apple Silicon），再将 `eja.app` 拖入 `/Applications`。

If macOS blocks the app on first launch:

首次打开若被 macOS 拦截：

```sh
xattr -dr com.apple.quarantine /Applications/eja.app
```

## Build from source / 从源码构建

```sh
./macos/scripts/build-app.sh
open macos/build/eja.app
```

See [core/README.md](core/README.md) for the identity log and core API, and [macos/README.md](macos/README.md) for macOS development details.

内核日志与 API 见 [core/README.md](core/README.md)；macOS 开发说明见 [macos/README.md](macos/README.md)。
