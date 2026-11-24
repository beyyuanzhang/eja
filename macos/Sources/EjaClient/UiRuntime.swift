import CryptoKit
import Foundation

public enum UiRuntime {
    private static let applyScript = """
    #!/usr/bin/env bash
    set -euo pipefail
    UI_DIR="$(cd "$(dirname "$0")" && pwd)"
    swift build --package-path "$UI_DIR" --product EjaUI --disable-sandbox
    BIN="$(swift build --package-path "$UI_DIR" --product EjaUI --disable-sandbox --show-bin-path)/libEjaUI.dylib"
    STAGE="$(mktemp "$UI_DIR/.EjaUI.XXXXXX")"
    trap 'rm -f "$STAGE"' EXIT
    cp "$BIN" "$STAGE"
    chmod 755 "$STAGE"
    codesign --force --sign - "$STAGE"
    mv "$STAGE" "$UI_DIR/libEjaUI.dylib"
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
        if !fm.fileExists(atPath: uiDir.appendingPathComponent("Package.swift").path) {
            guard let source = templateDir ?? resolveTemplateDir(),
                  (try? copyTemplate(from: source, to: uiDir)) != nil else { return false }
        }
        writeApplyScript(in: uiDir)
        return true
    }

    private static func copyTemplate(from source: URL, to target: URL) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        for item in ["Package.swift", "Sources", "Tests"] {
            let src = source.appendingPathComponent(item)
            guard fm.fileExists(atPath: src.path) else { continue }
            let dst = target.appendingPathComponent(item)
            if fm.fileExists(atPath: dst.path) { continue }
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
              let loaded = ProcessInfo.processInfo.environment["EJA_LOADED_UI_DIGEST"],
              let staged = binaryDigest(at: dataDir.appendingPathComponent("ui/libEjaUI.dylib")),
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
