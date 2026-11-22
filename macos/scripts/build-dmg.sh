#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MACOS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_DIR="$(cd "${MACOS_DIR}/.." && pwd)"
APP_DIR="${MACOS_DIR}/build/Eja.app"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${REPO_DIR}/Cargo.toml" | head -n 1)"
: "${VERSION:?Failed to read version from Cargo.toml}"
DMG_PATH="${MACOS_DIR}/build/Eja-${VERSION}.dmg"

if [ "${1:-}" != "--skip-build" ]; then
    "${SCRIPT_DIR}/build-app.sh"
fi

STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/eja-dmg.XXXXXX")"
trap 'rm -rf "${STAGE_DIR}"' EXIT

cp -R "${APP_DIR}" "${STAGE_DIR}/Eja.app"
ln -s /Applications "${STAGE_DIR}/Applications"

rm -f "${DMG_PATH}"
hdiutil create \
    -volname "EJA" \
    -srcfolder "${STAGE_DIR}" \
    -ov \
    -format UDZO \
    "${DMG_PATH}"

echo "Done: ${DMG_PATH}"
