import CryptoKit
import Foundation

public enum UiRuntime {
    private static let applyScript = """
    #!/usr/bin/env bash
    set -euo pipefail
    UI_DIR="$(cd "$(dirname "$0")" && pwd)"
    swift build --package-path "$UI_DIR" --product ejaUI --disable-sandbox
    BIN="$(swift build --package-path "$UI_DIR" --product ejaUI --disable-sandbox --show-bin-path)/libejaUI.dylib"
    STAGE="$(mktemp "$UI_DIR/.ejaUI.XXXXXX")"
    trap 'rm -f "$STAGE"' EXIT
    cp "$BIN" "$STAGE"
    chmod 755 "$STAGE"
    codesign --force --sign - "$STAGE"
    mv "$STAGE" "$UI_DIR/libejaUI.dylib"
    """

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
        if let source = templateDir ?? resolveTemplateDir() {
            guard (try? syncFactoryScaffold(from: source, to: uiDir, fm: fm)) != nil else {
                return false
            }
        } else if !fm.fileExists(atPath: uiDir.appendingPathComponent("Package.swift").path) {
            return false
        }
        writeApplyScript(in: uiDir)
        return true
    }

    private static func syncFactoryScaffold(from source: URL, to target: URL, fm: FileManager) throws {
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        let sourcesDir = target.appendingPathComponent("Sources", isDirectory: true)
        try fm.createDirectory(at: sourcesDir, withIntermediateDirectories: true)

        try replaceAtomically(
            from: source.appendingPathComponent("Package.swift"),
            to: target.appendingPathComponent("Package.swift"),
            fm: fm
        )
        try replaceAtomically(
            from: source.appendingPathComponent("Sources/ejaClient", isDirectory: true),
            to: sourcesDir.appendingPathComponent("ejaClient", isDirectory: true),
            fm: fm
        )
        let userAppDst = sourcesDir.appendingPathComponent("ejaApp", isDirectory: true)
        if !fm.fileExists(atPath: userAppDst.path) {
            let userAppSrc = source.appendingPathComponent("Sources/ejaApp", isDirectory: true)
            if fm.fileExists(atPath: userAppSrc.path) {
                try fm.copyItem(at: userAppSrc, to: userAppDst)
            }
        }
        let launcherSrc = source.appendingPathComponent("Sources/ejaLauncher", isDirectory: true)
        let launcherDst = sourcesDir.appendingPathComponent("ejaLauncher", isDirectory: true)
        if fm.fileExists(atPath: launcherSrc.path) {
            try replaceAtomically(from: launcherSrc, to: launcherDst, fm: fm)
        }
        let testsSrc = source.appendingPathComponent("Tests", isDirectory: true)
        let testsDst = target.appendingPathComponent("Tests", isDirectory: true)
        if fm.fileExists(atPath: testsSrc.path), !fm.fileExists(atPath: testsDst.path) {
            try fm.copyItem(at: testsSrc, to: testsDst)
        }
    }

    private static func replaceAtomically(from src: URL, to dst: URL, fm: FileManager) throws {
        guard fm.fileExists(atPath: src.path) else { return }
        let parent = dst.deletingLastPathComponent()
        let staging = parent.appendingPathComponent(".sync-\(UUID().uuidString)")
        do {
            try fm.copyItem(at: src, to: staging)
            if fm.fileExists(atPath: dst.path) {
                _ = try fm.replaceItemAt(dst, withItemAt: staging)
            } else {
                try fm.moveItem(at: staging, to: dst)
            }
        } catch {
            try? fm.removeItem(at: staging)
            throw error
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
            && fm.fileExists(atPath: url.appendingPathComponent("Sources/ejaApp").path)
    }

    private static func binaryDigest(at url: URL) -> String? {
        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return nil }
        let digest = SHA256.hash(data: data)
        return digest.map { String(format: "%02x", $0) }.joined()
    }

    @discardableResult
    public static func reloadIfNeeded(
        in dataDir: URL,
        baselineId: Int64,
        shutdown: () -> Void
    ) -> Bool {
        guard Bundle.main.bundleURL.pathExtension == "app",
              let loaded = ProcessInfo.processInfo.environment["eja_LOADED_UI_DIGEST"],
              let staged = binaryDigest(at: dataDir.appendingPathComponent("ui/libejaUI.dylib")),
              staged != loaded else { return false }
        if baselineId >= 0 {
            try? "\(baselineId)".write(
                to: reloadBaselineFile(in: dataDir),
                atomically: true,
                encoding: .utf8
            )
        }
        let pid = ProcessInfo.processInfo.processIdentifier
        let appPath = Bundle.main.bundleURL.path
        let relaunch = Process()
        relaunch.executableURL = URL(fileURLWithPath: "/bin/sh")
        relaunch.arguments = [
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.05; done; exec /usr/bin/open -n \"$2\" --args --data-dir \"$3\"",
            "--",
            "\(pid)",
            appPath,
            dataDir.path,
        ]
        do { try relaunch.run() } catch { return false }
        shutdown()
        exit(0)
    }

}
