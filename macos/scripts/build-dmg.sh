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

DMGBUILD_ENV="${MACOS_DIR}/build/.dmg-build"
if [ ! -x "${DMGBUILD_ENV}/bin/dmgbuild" ]; then
    python3 -m venv "${DMGBUILD_ENV}"
    "${DMGBUILD_ENV}/bin/python" -m pip install "dmgbuild==1.6.7"
fi

STAGE_DIR="$(mktemp -d "${MACOS_DIR}/build/.dmg.XXXXXX")"
trap 'rm -rf "${STAGE_DIR}"' EXIT

"${DMGBUILD_ENV}/bin/dmgbuild" -s "${SCRIPT_DIR}/dmg-layout.py" \
    -D "app=${APP_DIR}" -D "background=${MACOS_DIR}/Resources/DmgBackground.tiff" \
    "EJA" "${STAGE_DIR}/Eja.dmg"
hdiutil verify "${STAGE_DIR}/Eja.dmg"
mv "${STAGE_DIR}/Eja.dmg" "${DMG_PATH}"

echo "Done: ${DMG_PATH}"
