import Foundation

public struct BinaryStamp: Equatable, Sendable {
    let inode: UInt64
    let mtime: TimeInterval
}

public enum UiRuntime {
    private static let applyScript = """
    #!/usr/bin/env bash
    set -euo pipefail
    UI_DIR="$(cd "$(dirname "$0")" && pwd)"
    swift build --package-path "$UI_DIR" --product EjaApp --disable-sandbox
    BIN="$(swift build --package-path "$UI_DIR" --product EjaApp --disable-sandbox --show-bin-path)/EjaApp"
    STAGE="$UI_DIR/Eja.app.tmp"
    TARGET="$UI_DIR/Eja.app"
    rm -rf "$STAGE"
    mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"
    cp "$BIN" "$STAGE/Contents/MacOS/EjaApp"
    if [ -f "$UI_DIR/AppIcon.icns" ]; then
        cp "$UI_DIR/AppIcon.icns" "$STAGE/Contents/Resources/AppIcon.icns"
    fi
    if [ -f "$UI_DIR/Info.plist" ]; then
        cp "$UI_DIR/Info.plist" "$STAGE/Contents/Info.plist"
    else
        cat > "$STAGE/Contents/Info.plist" <<'EOF'
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
        <key>CFBundleExecutable</key><string>EjaApp</string>
        <key>CFBundleIconFile</key><string>AppIcon</string>
        <key>CFBundleIdentifier</key><string>ai.eja.origin</string>
        <key>CFBundleName</key><string>EJA</string>
        <key>CFBundlePackageType</key><string>APPL</string>
        <key>NSHighResolutionCapable</key><true/>
    </dict></plist>
    EOF
    fi
    codesign --force --sign - "$STAGE" >/dev/null 2>&1 || true
    rm -rf "$TARGET"
    mv "$STAGE" "$TARGET"
    """

    public static func evolvedAppBundle(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/Eja.app", isDirectory: true)
    }

    public static func evolvedExecutable(in dataDir: URL) -> URL {
        evolvedAppBundle(in: dataDir).appendingPathComponent("Contents/MacOS/EjaApp")
    }

    public static func resolveTemplateDir() -> URL? {
        let fm = FileManager.default
        if let res = Bundle.main.resourceURL?.appendingPathComponent("ui-template", isDirectory: true),
           isTemplateDir(res, fm: fm) {
            return res
        }
        var cursor = URL(fileURLWithPath: fm.currentDirectoryPath, isDirectory: true).standardizedFileURL
        for _ in 0..<8 {
            for candidate in [cursor, cursor.appendingPathComponent("macos", isDirectory: true)] {
                if isTemplateDir(candidate, fm: fm) { return candidate }
            }
            let parent = cursor.deletingLastPathComponent()
            if parent.path == cursor.path { break }
            cursor = parent
        }
        return nil
    }

    @discardableResult
    public static func bootstrapIfNeeded(dataDir: URL, templateDir: URL? = nil) -> Bool {
        let uiDir = dataDir.appendingPathComponent("ui", isDirectory: true)
        if !FileManager.default.fileExists(atPath: uiDir.appendingPathComponent("Package.swift").path) {
            guard let source = templateDir ?? resolveTemplateDir(),
                  (try? copyTemplate(from: source, to: uiDir)) != nil else { return false }
        }
        writeApplyScript(in: uiDir)
        return true
    }

    private static func copyTemplate(from source: URL, to target: URL) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        for item in ["Package.swift", "Sources", "Tests", "Info.plist", "AppIcon.icns"] {
            let src = source.appendingPathComponent(item)
            guard fm.fileExists(atPath: src.path) else { continue }
            let dst = target.appendingPathComponent(item)
            if fm.fileExists(atPath: dst.path) { try fm.removeItem(at: dst) }
            try fm.copyItem(at: src, to: dst)
        }
    }

    private static func writeApplyScript(in uiDir: URL) {
        let script = uiDir.appendingPathComponent("apply.sh")
        if !FileManager.default.fileExists(atPath: script.path) {
            try? applyScript.write(to: script, atomically: true, encoding: .utf8)
        }
        try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
    }

    private static func isTemplateDir(_ url: URL, fm: FileManager) -> Bool {
        fm.fileExists(atPath: url.appendingPathComponent("Package.swift").path)
            && fm.fileExists(atPath: url.appendingPathComponent("Sources/EjaApp").path)
    }

    public static func binaryStamp(at url: URL) -> BinaryStamp? {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
              let inode = (attrs[.systemFileNumber] as? NSNumber)?.uint64Value,
              let mtime = (attrs[.modificationDate] as? Date)?.timeIntervalSince1970 else { return nil }
        return BinaryStamp(inode: inode, mtime: mtime)
    }

}
