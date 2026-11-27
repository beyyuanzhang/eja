# eja for macOS

## App structure

- `Sources/ejaLauncher/` starts the app, loads the interface, and selects the data directory.
- `Sources/ejaClient/` communicates with the host and supports live interface updates.
- `Sources/ejaApp/` contains the SwiftUI chat interface, model settings, and capability controls.

## Build and run

Build the app bundle, then open it (system notifications are supported):

```sh
./macos/scripts/build-app.sh
open /Applications/eja.app
```

To create a `.dmg` installer:

```sh
./macos/scripts/build-dmg.sh
```

The packaging script requires Python 3.10 or later. It creates a virtual environment under `macos/build/` and installs the Finder layout tool there.

## Run the development build

From the repository root, build the host, then use SwiftPM to test and launch the development app:

```sh
cargo build -p eja-host
cd macos
export eja_HOST_BIN="$PWD/../target/debug/eja-host"
swift test
swift build --product ejaUI
swift run ejaApp
```

If you have Command Line Tools but not the full Xcode installation, load the Swift Testing macro plug-in explicitly when running tests:

```sh
swift test --disable-xctest --enable-swift-testing \
  -Xswiftc -load-plugin-library \
  -Xswiftc /Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing/libTestingMacros.dylib
```

For the host and data-directory details, see the [project README](../README.md).
