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
    cp "$BIN" "$UI_DIR/EjaApp.tmp"
    mv "$UI_DIR/EjaApp.tmp" "$UI_DIR/EjaApp"
    """

    public static func evolvedExecutable(in dataDir: URL) -> URL {
        dataDir.appendingPathComponent("ui/EjaApp")
    }

    public static func resolveTemplateDir() -> URL? {
        let fm = FileManager.default
        if let env = ProcessInfo.processInfo.environment["EJA_UI_TEMPLATE_DIR"], !env.isEmpty {
            let url = URL(fileURLWithPath: env, isDirectory: true)
            if isTemplateDir(url, fm: fm) { return url }
        }
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

    public static func runningStamp(in dataDir: URL) -> BinaryStamp? {
        let evolved = evolvedExecutable(in: dataDir)
        guard let current = currentExecutable(),
              current.resolvingSymlinksInPath().path == evolved.resolvingSymlinksInPath().path else {
            return nil
        }
        return binaryStamp(at: evolved)
    }

    public static func updatedExecutable(in dataDir: URL, runningStamp: BinaryStamp?) -> URL? {
        let evolved = evolvedExecutable(in: dataDir)
        guard FileManager.default.isExecutableFile(atPath: evolved.path),
              let latest = binaryStamp(at: evolved),
              latest != runningStamp else {
            return nil
        }
        return evolved
    }

    public static func resetToDefault(dataDir: URL, templateDir: URL? = nil) throws -> URL? {
        let uiDir = dataDir.appendingPathComponent("ui", isDirectory: true)
        guard let source = templateDir ?? resolveTemplateDir() else {
            throw NSError(domain: "UiRuntime", code: 1, userInfo: [NSLocalizedDescriptionKey: "找不到默认界面模板目录"])
        }
        try copyTemplate(from: source, to: uiDir)
        writeApplyScript(in: uiDir, overwrite: true)
        try? FileManager.default.removeItem(at: evolvedExecutable(in: dataDir))
        guard let path = ProcessInfo.processInfo.environment["EJA_FALLBACK_APP_BIN"], !path.isEmpty else {
            return nil
        }
        let fallback = URL(fileURLWithPath: path)
        guard FileManager.default.isExecutableFile(atPath: fallback.path),
              fallback.resolvingSymlinksInPath().path != currentExecutable()?.resolvingSymlinksInPath().path else {
            return nil
        }
        return fallback
    }

    public static func launch(executable: URL, dataDir: URL, hostExecutableURL: URL?) throws {
        let proc = Process()
        proc.executableURL = executable
        var env = ProcessInfo.processInfo.environment
        env["EJA_DATA_DIR"] = dataDir.path
        if let host = hostExecutableURL { env["EJA_HOST_BIN"] = host.path }
        if let tpl = resolveTemplateDir() { env["EJA_UI_TEMPLATE_DIR"] = tpl.path }
        if (env["EJA_FALLBACK_APP_BIN"] ?? "").isEmpty, let cur = currentExecutable() {
            env["EJA_FALLBACK_APP_BIN"] = cur.path
        }
        proc.environment = env
        try proc.run()
    }

    private static func copyTemplate(from source: URL, to target: URL) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        for item in ["Package.swift", "Sources", "Tests"] {
            let src = source.appendingPathComponent(item)
            guard fm.fileExists(atPath: src.path) else { continue }
            let dst = target.appendingPathComponent(item)
            if fm.fileExists(atPath: dst.path) { try fm.removeItem(at: dst) }
            try fm.copyItem(at: src, to: dst)
        }
    }

    private static func writeApplyScript(in uiDir: URL, overwrite: Bool = false) {
        let script = uiDir.appendingPathComponent("apply.sh")
        if overwrite || !FileManager.default.fileExists(atPath: script.path) {
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

    private static func currentExecutable() -> URL? {
        Bundle.main.executableURL ?? CommandLine.arguments.first.map { URL(fileURLWithPath: $0) }
    }
}
