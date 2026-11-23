import CryptoKit
import Foundation

public enum UiRuntime {
    private static let applyScript = """
    #!/usr/bin/env bash
    set -euo pipefail
    UI_DIR="$(cd "$(dirname "$0")" && pwd)"
    swift build --package-path "$UI_DIR" --product EjaApp --disable-sandbox
    BIN="$(swift build --package-path "$UI_DIR" --product EjaApp --disable-sandbox --show-bin-path)/EjaApp"
    rm -rf "$UI_DIR/Eja.app" "$UI_DIR/Eja.app.tmp"
    STAGE="$UI_DIR/EjaUI.tmp"
    TARGET="$UI_DIR/EjaUI"
    rm -f "$STAGE"
    cp "$BIN" "$STAGE"
    chmod 755 "$STAGE"
    codesign --force --sign - "$STAGE" >/dev/null 2>&1 || true
    mv "$STAGE" "$TARGET"
    """

    public static func evolvedExecutable(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/EjaUI")
    }

    public static func reloadBaselineFile(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/.reload-baseline")
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
        let fm = FileManager.default
        if !fm.fileExists(atPath: uiDir.appendingPathComponent("Package.swift").path) {
            guard let source = templateDir ?? resolveTemplateDir(),
                  (try? copyTemplate(from: source, to: uiDir)) != nil else { return false }
        }
        let legacyBundle = uiDir.appendingPathComponent("Eja.app", isDirectory: true)
        if fm.fileExists(atPath: legacyBundle.path) {
            let legacyExec = legacyBundle.appendingPathComponent("Contents/MacOS/EjaApp")
            let targetExec = evolvedExecutable(in: dataDir)
            if fm.isExecutableFile(atPath: legacyExec.path) && !fm.fileExists(atPath: targetExec.path) {
                try? fm.copyItem(at: legacyExec, to: targetExec)
            }
            try? fm.removeItem(at: legacyBundle)
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
        let existing = try? String(contentsOf: script, encoding: .utf8)
        if existing != applyScript {
            try? applyScript.write(to: script, atomically: true, encoding: .utf8)
        }
        try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
    }

    private static func isTemplateDir(_ url: URL, fm: FileManager) -> Bool {
        fm.fileExists(atPath: url.appendingPathComponent("Package.swift").path)
            && fm.fileExists(atPath: url.appendingPathComponent("Sources/EjaApp").path)
    }

    public static func binaryDigest(at url: URL) -> String? {
        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return nil }
        let digest = SHA256.hash(data: data)
        return digest.map { String(format: "%02x", $0) }.joined()
    }

    public static func needsHotSwap(in dataDir: URL) -> Bool {
        guard Bundle.main.bundleURL.pathExtension == "app",
              let currentExec = Bundle.main.executableURL else { return false }
        let staged = evolvedExecutable(in: dataDir)
        guard FileManager.default.isExecutableFile(atPath: staged.path),
              let stagedDigest = binaryDigest(at: staged),
              let currentDigest = binaryDigest(at: currentExec) else { return false }
        return stagedDigest != currentDigest
    }

    @discardableResult
    public static func restoreFactoryIfNeeded(in dataDir: URL, shutdown: () -> Void) -> Bool {
        guard Bundle.main.bundleURL.pathExtension == "app",
              let currentExec = Bundle.main.executableURL else { return false }
        let fm = FileManager.default
        let factoryExec = currentExec.deletingLastPathComponent().appendingPathComponent("EjaApp.factory")
        let staged = evolvedExecutable(in: dataDir)
        try? fm.removeItem(at: staged)
        guard fm.isExecutableFile(atPath: factoryExec.path),
              let factoryDigest = binaryDigest(at: factoryExec),
              let currentDigest = binaryDigest(at: currentExec),
              factoryDigest != currentDigest else { return false }
        return replaceAndRelaunch(sourceBinary: factoryExec, currentExec: currentExec, shutdown: shutdown)
    }

    @discardableResult
    public static func installAndRelaunch(
        in dataDir: URL,
        baselineId: Int64,
        shutdown: () -> Void
    ) -> Bool {
        guard Bundle.main.bundleURL.pathExtension == "app",
              let currentExec = Bundle.main.executableURL else { return false }
        let staged = evolvedExecutable(in: dataDir)
        let fm = FileManager.default
        guard fm.isExecutableFile(atPath: staged.path) else { return false }
        let factoryExec = currentExec.deletingLastPathComponent().appendingPathComponent("EjaApp.factory")
        if !fm.fileExists(atPath: factoryExec.path) {
            try? fm.copyItem(at: currentExec, to: factoryExec)
        }
        if baselineId >= 0 {
            try? "\(baselineId)".write(
                to: reloadBaselineFile(in: dataDir),
                atomically: true,
                encoding: .utf8
            )
        }
        return replaceAndRelaunch(sourceBinary: staged, currentExec: currentExec, shutdown: shutdown)
    }

    private static func replaceAndRelaunch(
        sourceBinary: URL,
        currentExec: URL,
        shutdown: () -> Void
    ) -> Bool {
        let fm = FileManager.default
        let tmpExec = currentExec.deletingLastPathComponent().appendingPathComponent("EjaApp.next")
        try? fm.removeItem(at: tmpExec)
        do {
            try fm.copyItem(at: sourceBinary, to: tmpExec)
            try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: tmpExec.path)
            let sign = Process()
            sign.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
            sign.arguments = ["--force", "--sign", "-", tmpExec.path]
            try? sign.run()
            sign.waitUntilExit()
            if sourceBinary != currentExec {
                try? fm.removeItem(at: sourceBinary)
                try? fm.copyItem(at: tmpExec, to: sourceBinary)
            }
            _ = try fm.replaceItemAt(currentExec, withItemAt: tmpExec)
        } catch {
            try? fm.removeItem(at: tmpExec)
            return false
        }

        shutdown()
        let pid = ProcessInfo.processInfo.processIdentifier
        let appPath = Bundle.main.bundleURL.path
        let relaunch = Process()
        relaunch.executableURL = URL(fileURLWithPath: "/bin/sh")
        relaunch.arguments = [
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.05; done; open \"$2\"",
            "--",
            "\(pid)",
            appPath,
        ]
        try? relaunch.run()
        exit(0)
    }

}
