import CoreGraphics
import CryptoKit
import Darwin
import Foundation

private func binaryDigest(at url: URL) -> String? {
    (try? Data(contentsOf: url, options: .mappedIfSafe)).map {
        SHA256.hash(data: $0).map { String(format: "%02x", $0) }.joined()
    }
}

private func resolveDataDir() -> URL {
    let args = CommandLine.arguments
    if let idx = args.firstIndex(of: "--data-dir"), idx + 1 < args.count {
        return URL(fileURLWithPath: args[idx + 1], isDirectory: true)
    }
    if let env = ProcessInfo.processInfo.environment["eja_DATA_DIR"], !env.isEmpty {
        return URL(fileURLWithPath: env, isDirectory: true)
    }
    return FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/eja", isDirectory: true)
}

private func isSafeModeRequested() -> Bool {
    if ProcessInfo.processInfo.environment["eja_SAFE_MODE"] == "1" {
        return true
    }
    return CGEventSource.flagsState(.combinedSessionState).contains(.maskAlternate)
}

private func syncFactoryClient(from templateClientDir: URL, into uiDir: URL, fm: FileManager) -> Bool {
    guard fm.fileExists(atPath: templateClientDir.path) else { return false }
    let sourcesDir = uiDir.appendingPathComponent("Sources", isDirectory: true)
    let targetClientDir = sourcesDir.appendingPathComponent("ejaClient", isDirectory: true)
    let stagingDir = sourcesDir.appendingPathComponent(".ejaClient-sync-\(UUID().uuidString)", isDirectory: true)
    do {
        try fm.createDirectory(at: sourcesDir, withIntermediateDirectories: true)
        try fm.copyItem(at: templateClientDir, to: stagingDir)
        if fm.fileExists(atPath: targetClientDir.path) {
            _ = try fm.replaceItemAt(targetClientDir, withItemAt: stagingDir)
        } else {
            try fm.moveItem(at: stagingDir, to: targetClientDir)
        }
        return true
    } catch {
        try? fm.removeItem(at: stagingDir)
        return false
    }
}

private func rebuildEvolvedUI(applyScript: URL, fm: FileManager) -> String? {
    guard fm.isExecutableFile(atPath: applyScript.path) else {
        return "未找到可执行的 ui/apply.sh。"
    }
    let process = Process()
    process.executableURL = applyScript
    let pipe = Pipe()
    process.standardOutput = pipe
    process.standardError = pipe
    do {
        try process.run()
    } catch {
        return "无法执行 ui/apply.sh：\(error.localizedDescription)"
    }
    let outputData = pipe.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    if process.terminationStatus == 0 {
        return nil
    }
    let rawOutput = String(data: outputData, encoding: .utf8)?
        .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    let tail = rawOutput.count > 1500 ? String(rawOutput.suffix(1500)) : rawOutput
    if tail.isEmpty {
        return "ui/apply.sh 编译失败（退出码 \(process.terminationStatus)）。"
    }
    return "应用更新后自动重编 ui/ 失败（退出码 \(process.terminationStatus)）：\n\(tail)"
}

private func setFallbackReason(_ reason: String) {
    setenv("eja_UI_FALLBACK_REASON", reason, 1)
}

@discardableResult
private func loadAndRun(dylibURL: URL, digest: String) -> Bool {
    guard let handle = dlopen(dylibURL.path, RTLD_NOW | RTLD_LOCAL) else { return false }
    guard let sym = dlsym(handle, "eja_ui_main") else {
        dlclose(handle)
        return false
    }
    setenv("eja_LOADED_UI_DIGEST", digest, 1)
    typealias EntryPoint = @convention(c) () -> Void
    let entry = unsafeBitCast(sym, to: EntryPoint.self)
    entry()
    return true
}

let fm = FileManager.default
let execURL = (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0]))
    .resolvingSymlinksInPath()
let factoryDylib = execURL.deletingLastPathComponent().appendingPathComponent("libejaUI.dylib")
let dataDir = resolveDataDir().standardizedFileURL
setenv("eja_DATA_DIR", dataDir.path, 1)
let uiDir = dataDir.appendingPathComponent("ui", isDirectory: true)
let evolvedDylib = uiDir.appendingPathComponent("libejaUI.dylib")
let stampFile = uiDir.appendingPathComponent(".factory-digest")
let bootMarker = uiDir.appendingPathComponent(".booting")
let applyScript = uiDir.appendingPathComponent("apply.sh")
let templateClientDir = Bundle.main.resourceURL?
    .appendingPathComponent("ui-template/Sources/ejaClient", isDirectory: true)

let safeMode = isSafeModeRequested()
setenv("eja_SAFE_MODE", safeMode ? "1" : "0", 1)
if safeMode {
    try? fm.removeItem(at: evolvedDylib)
    try? fm.removeItem(at: bootMarker)
}
let factoryDigest = binaryDigest(at: factoryDylib)
var canLoadEvolved = !safeMode
if canLoadEvolved && fm.fileExists(atPath: bootMarker.path) {
    try? fm.removeItem(at: bootMarker)
    try? fm.removeItem(at: evolvedDylib)
    setFallbackReason("上次加载自定义界面时异常退出，已回退到出厂界面。请检查 ui/Sources/ejaApp 后重新运行 ui/apply.sh。")
    canLoadEvolved = false
}

if let factoryDigest {
    let recorded = (try? String(contentsOf: stampFile, encoding: .utf8))?
        .trimmingCharacters(in: .whitespacesAndNewlines)
    if recorded != factoryDigest {
        if canLoadEvolved && fm.fileExists(atPath: evolvedDylib.path) {
            if let templateClientDir {
                _ = syncFactoryClient(from: templateClientDir, into: uiDir, fm: fm)
            }
            if let failureMessage = rebuildEvolvedUI(applyScript: applyScript, fm: fm) {
                try? fm.removeItem(at: evolvedDylib)
                try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
                setFallbackReason(failureMessage)
                canLoadEvolved = false
            } else {
                try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
            }
        } else {
            try? fm.removeItem(at: evolvedDylib)
            if !fm.fileExists(atPath: evolvedDylib.path) {
                try? fm.createDirectory(at: uiDir, withIntermediateDirectories: true)
                try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
            }
        }
    }

    if canLoadEvolved, let digest = binaryDigest(at: evolvedDylib) {
        try? fm.createDirectory(at: uiDir, withIntermediateDirectories: true)
        try? digest.write(to: bootMarker, atomically: true, encoding: .utf8)
        if loadAndRun(dylibURL: evolvedDylib, digest: digest) { exit(0) }
        try? fm.removeItem(at: bootMarker)
        try? fm.removeItem(at: evolvedDylib)
        setFallbackReason("加载 ui/libejaUI.dylib 失败，已回退到出厂界面。请检查 ui/ 后重新运行 ui/apply.sh。")
    }
}

if let factoryDigest, loadAndRun(dylibURL: factoryDylib, digest: factoryDigest) {
    exit(0)
} else {
    fputs("Failed to load eja UI runtime at \(factoryDylib.path)\n", stderr)
    exit(1)
}
