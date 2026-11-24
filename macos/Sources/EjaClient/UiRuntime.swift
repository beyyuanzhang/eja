import CryptoKit
import Foundation

public enum UiRuntime {
    private static let applyScript = """
    #!/usr/bin/env bash
    set -euo pipefail
    UI_DIR="$(cd "$(dirname "$0")" && pwd)"
    swift build --package-path "$UI_DIR" --product EjaUI --disable-sandbox
    BIN="$(swift build --package-path "$UI_DIR" --product EjaUI --disable-sandbox --show-bin-path)/libEjaUI.dylib"
    rm -rf "$UI_DIR/Eja.app" "$UI_DIR/Eja.app.tmp" "$UI_DIR/EjaUI" "$UI_DIR/EjaUI.tmp"
    STAGE="$UI_DIR/libEjaUI.dylib.tmp"
    TARGET="$UI_DIR/libEjaUI.dylib"
    rm -f "$STAGE"
    cp "$BIN" "$STAGE"
    chmod 755 "$STAGE"
    codesign --force --sign - "$STAGE" >/dev/null 2>&1 || true
    mv "$STAGE" "$TARGET"
    """

    public static func evolvedExecutable(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/libEjaUI.dylib")
    }

    public static func reloadBaselineFile(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/.reload-baseline")
    }

    public static func factoryDigestFile(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/.factory-digest")
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
        let pkgURL = uiDir.appendingPathComponent("Package.swift")
        let pkgText = try? String(contentsOf: pkgURL, encoding: .utf8)
        let needsTemplateSync = !(pkgText?.contains("name: \"EjaUI\"") ?? false)
        if needsTemplateSync {
            guard let source = templateDir ?? resolveTemplateDir(),
                  (try? copyTemplate(from: source, to: uiDir)) != nil else { return false }
        }
        let legacyBundle = uiDir.appendingPathComponent("Eja.app", isDirectory: true)
        if fm.fileExists(atPath: legacyBundle.path) {
            try? fm.removeItem(at: legacyBundle)
        }
        try? fm.removeItem(at: uiDir.appendingPathComponent("EjaUI"))
        if let factoryDigest = ProcessInfo.processInfo.environment["EJA_FACTORY_UI_DIGEST"],
           !factoryDigest.isEmpty {
            try? factoryDigest.write(to: factoryDigestFile(in: dataDir), atomically: true, encoding: .utf8)
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
        guard Bundle.main.bundleURL.pathExtension == "app" else { return false }
        let staged = evolvedExecutable(in: dataDir)
        guard FileManager.default.fileExists(atPath: staged.path),
              let stagedDigest = binaryDigest(at: staged) else { return false }
        let loadedDigest = ProcessInfo.processInfo.environment["EJA_LOADED_UI_DIGEST"]
            ?? Bundle.main.executableURL.flatMap {
                binaryDigest(at: $0.deletingLastPathComponent().appendingPathComponent("libEjaUI.dylib"))
            }
        guard let currentDigest = loadedDigest else { return false }
        return stagedDigest != currentDigest
    }

    @discardableResult
    public static func restoreFactoryIfNeeded(in dataDir: URL, shutdown: () -> Void) -> Bool {
        let fm = FileManager.default
        let staged = evolvedExecutable(in: dataDir)
        let hadStaged = fm.fileExists(atPath: staged.path)
        try? fm.removeItem(at: staged)
        let loadedDigest = ProcessInfo.processInfo.environment["EJA_LOADED_UI_DIGEST"]
        let factoryDigest = ProcessInfo.processInfo.environment["EJA_FACTORY_UI_DIGEST"]
        guard Bundle.main.bundleURL.pathExtension == "app",
              hadStaged || (loadedDigest != nil && factoryDigest != nil && loadedDigest != factoryDigest) else {
            return false
        }
        return relaunchApp(shutdown: shutdown)
    }

    @discardableResult
    public static func installAndRelaunch(
        in dataDir: URL,
        baselineId: Int64,
        shutdown: () -> Void
    ) -> Bool {
        guard Bundle.main.bundleURL.pathExtension == "app" else { return false }
        let staged = evolvedExecutable(in: dataDir)
        let fm = FileManager.default
        guard fm.fileExists(atPath: staged.path) else { return false }
        if let factoryDigest = ProcessInfo.processInfo.environment["EJA_FACTORY_UI_DIGEST"],
           !factoryDigest.isEmpty {
            try? factoryDigest.write(to: factoryDigestFile(in: dataDir), atomically: true, encoding: .utf8)
        }
        if baselineId >= 0 {
            try? "\(baselineId)".write(
                to: reloadBaselineFile(in: dataDir),
                atomically: true,
                encoding: .utf8
            )
        }
        return relaunchApp(shutdown: shutdown)
    }

    private static func relaunchApp(shutdown: () -> Void) -> Bool {
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
