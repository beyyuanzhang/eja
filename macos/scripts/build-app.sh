#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MACOS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_DIR="$(cd "${MACOS_DIR}/.." && pwd)"
APP_DIR="${1:-/Applications/eja.app}"
MODEL_DIR="${MACOS_DIR}/build/memory-model"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${REPO_DIR}/Cargo.toml" | head -n 1)"
: "${VERSION:?Failed to read version from Cargo.toml}"

"${REPO_DIR}/memory/scripts/install-memory-model.sh" "${MODEL_DIR}"

echo "[1/3] Building eja-host (Rust release)..."
(cd "${REPO_DIR}" && MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -p eja-host)

echo "[2/3] Building ejaApp & ejaUI (SwiftPM release)..."
(cd "${MACOS_DIR}" && swift build -c release -debug-info-format none --product ejaApp --disable-sandbox)
(cd "${MACOS_DIR}" && swift build -c release -debug-info-format none --product ejaUI --disable-sandbox)

APP_BIN="${MACOS_DIR}/.build/release/ejaApp"
UI_DYLIB="${MACOS_DIR}/.build/release/libejaUI.dylib"
HOST_BIN="${REPO_DIR}/target/release/eja-host"

echo "[3/3] Assembling ${APP_DIR}..."
APP_PARENT="$(dirname "${APP_DIR}")"
mkdir -p "${APP_PARENT}"
STAGE_DIR="$(mktemp -d "${APP_PARENT}/.eja-build.XXXXXX")"
STAGE_APP="${STAGE_DIR}/eja.app"
BACKUP_APP="${STAGE_DIR}/previous"
cleanup() {
    if [ -e "${BACKUP_APP}" ] || [ -L "${BACKUP_APP}" ]; then
        if [ ! -e "${APP_DIR}" ] && [ ! -L "${APP_DIR}" ]; then
            if ! mv "${BACKUP_APP}" "${APP_DIR}"; then
                echo "Previous app preserved at ${BACKUP_APP}" >&2
                return 1
            fi
        fi
    fi
    rm -rf "${STAGE_DIR}"
}
trap cleanup EXIT
mkdir -p "${STAGE_APP}/Contents/MacOS" "${STAGE_APP}/Contents/Resources/ui-template"
MODEL_RESOURCES="${STAGE_APP}/Contents/Resources/memory-model"
mkdir -p "${MODEL_RESOURCES}"
install -m 644 "${MODEL_DIR}"/{config.json,encoder.json,model_quantized.onnx,special_tokens_map.json,tokenizer.json,tokenizer_config.json} \
    "${REPO_DIR}/memory/scripts/multilingual-e5.LICENSE" "${MODEL_RESOURCES}/"
cp "${APP_BIN}" "${STAGE_APP}/Contents/MacOS/ejaApp"
cp "${UI_DYLIB}" "${STAGE_APP}/Contents/MacOS/libejaUI.dylib"
cp "${HOST_BIN}" "${STAGE_APP}/Contents/MacOS/eja-host"
cp "${REPO_DIR}/PROTOCOL.md" "${STAGE_APP}/Contents/Resources/PROTOCOL.md"
cp "${MACOS_DIR}/Resources/AppIcon.icns" "${STAGE_APP}/Contents/Resources/AppIcon.icns"

cat > "${STAGE_APP}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleExecutable</key>
    <string>ejaApp</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>ai.eja.app</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>eja</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
cp -R "${MACOS_DIR}/Package.swift" "${MACOS_DIR}/Sources" "${MACOS_DIR}/Tests" \
    "${STAGE_APP}/Contents/Resources/ui-template/"
find "${STAGE_APP}/Contents/Resources/ui-template" -type f -name .DS_Store -delete
codesign --force --deep --sign - "${STAGE_APP}"
codesign --verify --deep --strict "${STAGE_APP}"
if [ -e "${APP_DIR}" ] || [ -L "${APP_DIR}" ]; then
    mv "${APP_DIR}" "${BACKUP_APP}"
fi
mv "${STAGE_APP}" "${APP_DIR}"

echo "Done: ${APP_DIR}"
echo "Run with: open \"${APP_DIR}\""
