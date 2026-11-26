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

let safeMode = isSafeModeRequested()
setenv("eja_SAFE_MODE", safeMode ? "1" : "0", 1)
if safeMode {
    try? fm.removeItem(at: evolvedDylib)
}
let factoryDigest = binaryDigest(at: factoryDylib)
if let factoryDigest {
    let recorded = (try? String(contentsOf: stampFile, encoding: .utf8))?
        .trimmingCharacters(in: .whitespacesAndNewlines)
    if recorded == factoryDigest {
        if !safeMode, let digest = binaryDigest(at: evolvedDylib) {
            if loadAndRun(dylibURL: evolvedDylib, digest: digest) { exit(0) }
            try? fm.removeItem(at: evolvedDylib)
        }
    } else {
        try? fm.removeItem(at: evolvedDylib)
        if !fm.fileExists(atPath: evolvedDylib.path) {
            try? fm.createDirectory(at: uiDir, withIntermediateDirectories: true)
            try? factoryDigest.write(to: stampFile, atomically: true, encoding: .utf8)
        }
    }
}

if let factoryDigest, loadAndRun(dylibURL: factoryDylib, digest: factoryDigest) {
    exit(0)
} else {
    fputs("Failed to load eja UI runtime at \(factoryDylib.path)\n", stderr)
    exit(1)
}
