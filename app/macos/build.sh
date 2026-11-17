#!/bin/zsh
set -euo pipefail
cd "$(dirname "$0")"

# The CLT toolchain ships no SwiftUIMacros plugin, which the macOS 27 SDK's
# SwiftUI interface requires for @State. The macOS 26 SDK still exposes the
# classic property wrappers, so build against it with the native build system.
SDK="$(dirname "$(xcrun --show-sdk-path)")/MacOSX26.sdk"
if [[ -d "$SDK" ]]; then
    swift build -c release --build-system native --sdk "$SDK"
else
    swift build -c release
fi

APP=dist/Eja.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp .build/release/Eja "$APP/Contents/MacOS/Eja"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>Eja</string>
    <key>CFBundleIdentifier</key><string>dev.eja.macos</string>
    <key>CFBundleName</key><string>Eja</string>
    <key>CFBundleDisplayName</key><string>Eja</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

codesign --force --sign - "$APP" >/dev/null 2>&1 || true
echo "built $APP"
