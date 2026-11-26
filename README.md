# eja

**eja is an open-source, sessionless harness for a continuously growing AI identity—one each person can shape for themselves.**

The harness provides continuity; each person chooses how their eja remembers, what it can do, and how it works. Its history and tools can be extended over time, so the same identity grows through use instead of starting over with each session.

## 中文

**eja 是一个开源、无 session 的 harness：让 AI 身份持续成长，并由每个人按自己的方式定制。**

Harness 提供连续性；每个人都可以决定自己的 eja 如何记忆、能做什么、怎样工作。它的经历和能力可以随使用不断扩展，让同一个身份持续成长，而不是每次从新 session 开始。

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
