import AppKit
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

private func replaceAtomically(from src: URL, to dst: URL, fm: FileManager) -> Bool {
    guard fm.fileExists(atPath: src.path) else { return false }
    let parent = dst.deletingLastPathComponent()
    let staging = parent.appendingPathComponent(".sync-\(UUID().uuidString)")
    do {
        try fm.createDirectory(at: parent, withIntermediateDirectories: true)
        try fm.copyItem(at: src, to: staging)
        if fm.fileExists(atPath: dst.path) {
            _ = try fm.replaceItemAt(dst, withItemAt: staging)
        } else {
            try fm.moveItem(at: staging, to: dst)
        }
        return true
    } catch {
        try? fm.removeItem(at: staging)
        return false
    }
}

private func syncFactoryFiles(from templateDir: URL, into uiDir: URL, fm: FileManager) {
    _ = replaceAtomically(
        from: templateDir.appendingPathComponent("Package.swift"),
        to: uiDir.appendingPathComponent("Package.swift"),
        fm: fm
    )
    _ = replaceAtomically(
        from: templateDir.appendingPathComponent("Sources/ejaClient", isDirectory: true),
        to: uiDir.appendingPathComponent("Sources/ejaClient", isDirectory: true),
        fm: fm
    )
    _ = replaceAtomically(
        from: templateDir.appendingPathComponent("Sources/ejaLauncher", isDirectory: true),
        to: uiDir.appendingPathComponent("Sources/ejaLauncher", isDirectory: true),
        fm: fm
    )
}

private func collectRegularFiles(under root: URL, fm: FileManager) -> [String: URL]? {
    guard fm.fileExists(atPath: root.path),
          let enumerator = fm.enumerator(
              at: root,
              includingPropertiesForKeys: [.isRegularFileKey],
              options: [.skipsHiddenFiles]
          ) else {
        return nil
    }
    let basePath = root.standardizedFileURL.path
    var files: [String: URL] = [:]
    for case let url as URL in enumerator {
        guard (try? url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true else {
            continue
        }
        let fullPath = url.standardizedFileURL.path
        let rel = fullPath.hasPrefix(basePath + "/")
            ? String(fullPath.dropFirst(basePath.count + 1))
            : url.lastPathComponent
        files[rel] = url
    }
    return files
}

private func hasCustomizedApp(uiDir: URL, templateDir: URL?, fm: FileManager) -> Bool {
    let userApp = uiDir.appendingPathComponent("Sources/ejaApp", isDirectory: true)
    guard let userFiles = collectRegularFiles(under: userApp, fm: fm), !userFiles.isEmpty else {
        return false
    }
    guard let templateDir,
          let factoryFiles = collectRegularFiles(
              under: templateDir.appendingPathComponent("Sources/ejaApp", isDirectory: true),
              fm: fm
          ) else {
        return true
    }
    if Set(userFiles.keys) != Set(factoryFiles.keys) {
        return true
    }
    for (rel, userURL) in userFiles {
        guard let factoryURL = factoryFiles[rel],
              let userData = try? Data(contentsOf: userURL),
              let factoryData = try? Data(contentsOf: factoryURL),
              userData == factoryData else {
            return true
        }
    }
    return false
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
let templateDir = Bundle.main.resourceURL?
    .appendingPathComponent("ui-template", isDirectory: true)

let safeMode = isSafeModeRequested()
setenv("eja_SAFE_MODE", safeMode ? "1" : "0", 1)
if safeMode {
    try? fm.removeItem(at: bootMarker)
}
let factoryDigest = binaryDigest(at: factoryDylib)
let evolvedDigest = binaryDigest(at: evolvedDylib)
var canLoadEvolved = !safeMode
if canLoadEvolved, let previousBoot = try? String(contentsOf: bootMarker, encoding: .utf8) {
    try? fm.removeItem(at: bootMarker)
    if let evolvedDigest, previousBoot == "launching:\(evolvedDigest)" {
        setFallbackReason("上次启动自定义界面时异常退出，本次使用出厂界面。")
        canLoadEvolved = false
    }
}

if let factoryDigest {
    let recorded = (try? String(contentsOf: stampFile, encoding: .utf8))?
        .trimmingCharacters(in: .whitespacesAndNewlines)
    let customized = hasCustomizedApp(uiDir: uiDir, templateDir: templateDir, fm: fm)
    let needsSync = recorded != factoryDigest
    let needsRebuild = canLoadEvolved && customized && (needsSync || !fm.fileExists(atPath: evolvedDylib.path))
    if needsRebuild {
        if let templateDir {
            syncFactoryFiles(from: templateDir, into: uiDir, fm: fm)
        }
        if let failureMessage = rebuildEvolvedUI(applyScript: applyScript, fm: fm) {
            try? fm.removeItem(at: evolvedDylib)
            try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
            setFallbackReason(failureMessage)
            canLoadEvolved = false
        } else {
            try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
        }
    } else if canLoadEvolved && needsSync {
        try? fm.removeItem(at: evolvedDylib)
        if fm.fileExists(atPath: uiDir.path) {
            try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
        }
    }

    if canLoadEvolved, let digest = binaryDigest(at: evolvedDylib) {
        try? "launching:\(digest)".write(to: bootMarker, atomically: true, encoding: .utf8)
        let notifications = NotificationCenter.default
        let bootObserver = notifications.addObserver(
            forName: NSApplication.didFinishLaunchingNotification, object: nil, queue: nil
        ) { _ in
            try? FileManager.default.removeItem(at: bootMarker)
        }
        let loaded = loadAndRun(dylibURL: evolvedDylib, digest: digest)
        notifications.removeObserver(bootObserver)
        try? fm.removeItem(at: bootMarker)
        if loaded { exit(0) }
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
