# EJA macOS

## 结构

- `Sources/EjaLauncher/` 加载 EJA 界面并选择数据目录。
- `Sources/EjaClient/` 负责宿主通信和界面热加载。
- `Sources/EjaApp/` 提供 SwiftUI 对话界面、模型设置和能力管理。

## 构建与运行

从仓库根目录打包并运行 `.app`（支持系统通知）：

```sh
./macos/scripts/build-app.sh
open macos/build/Eja.app
```

生成拖拽安装的 `.dmg` 安装包：

```sh
./macos/scripts/build-dmg.sh
```

在仓库根目录运行以下命令，用 SwiftPM 测试并启动开发版：

```sh
cargo build -p eja-host
cd macos
export EJA_HOST_BIN="$PWD/../target/debug/eja-host"
swift test
swift build --product EjaUI
swift run EjaApp
```

若仅安装了 Command Line Tools（未安装完整 Xcode），执行 Swift Testing 测试时需显式加载宏插件：

```sh
swift test --disable-xctest --enable-swift-testing \
  -Xswiftc -load-plugin-library \
  -Xswiftc /Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing/libTestingMacros.dylib
```

宿主和数据目录见 [EJA 项目说明](../README.md)。
