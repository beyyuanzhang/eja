#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MACOS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_DIR="$(cd "${MACOS_DIR}/.." && pwd)"
APP_DIR="${MACOS_DIR}/build/Eja.app"

echo "[1/3] Building eja-host (Rust release)..."
(cd "${REPO_DIR}" && MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -p eja-host)

echo "[2/3] Building EjaApp (SwiftPM release)..."
(cd "${MACOS_DIR}" && swift build -c release -debug-info-format none --product EjaApp --disable-sandbox)

APP_BIN="${MACOS_DIR}/.build/release/EjaApp"
HOST_BIN="${REPO_DIR}/target/release/eja-host"

echo "[3/3] Assembling ${APP_DIR}..."
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"
cp "${APP_BIN}" "${APP_DIR}/Contents/MacOS/EjaApp"
cp "${HOST_BIN}" "${APP_DIR}/Contents/MacOS/eja-host"

cat > "${APP_DIR}/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleExecutable</key>
    <string>EjaApp</string>
    <key>CFBundleIdentifier</key>
    <string>ai.eja.origin</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>EJA</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

echo "Done: ${APP_DIR}"
echo "Run with: open \"${APP_DIR}\""
