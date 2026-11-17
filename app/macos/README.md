# Eja for macOS

Build: `./build.sh` (swift build -c release, assembles `dist/Eja.app`).
Run: `open dist/Eja.app` — the eja runtime must be serving (e.g. `cargo run -- serve --dir .eja --project .` at the repo root).
Configure: ⌘, opens settings; the runtime address defaults to `http://127.0.0.1:8787` and can be changed in 设置 → 运行 → 连接.
