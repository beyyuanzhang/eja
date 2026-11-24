import CoreGraphics
import CryptoKit
import Darwin
import Foundation

private func sha256(at url: URL) -> String? {
    guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return nil }
    return SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

private func resolveDataDir() -> URL {
    let args = CommandLine.arguments
    if let idx = args.firstIndex(of: "--data-dir"), idx + 1 < args.count {
        return URL(fileURLWithPath: args[idx + 1], isDirectory: true)
    }
    if let env = ProcessInfo.processInfo.environment["EJA_DATA_DIR"], !env.isEmpty {
        return URL(fileURLWithPath: env, isDirectory: true)
    }
    return FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/Eja", isDirectory: true)
}

private func isSafeModeRequested() -> Bool {
    if ProcessInfo.processInfo.environment["EJA_SAFE_MODE"] == "1" {
        return true
    }
    return CGEventSource.flagsState(.combinedSessionState).contains(.maskAlternate)
}

@discardableResult
private func loadAndRun(dylibURL: URL) -> Bool {
    guard FileManager.default.fileExists(atPath: dylibURL.path),
          let handle = dlopen(dylibURL.path, RTLD_NOW | RTLD_GLOBAL),
          let sym = dlsym(handle, "eja_ui_main") else {
        return false
    }
    if let digest = sha256(at: dylibURL) {
        setenv("EJA_LOADED_UI_DIGEST", digest, 1)
    }
    typealias EntryPoint = @convention(c) () -> Void
    let entry = unsafeBitCast(sym, to: EntryPoint.self)
    entry()
    return true
}

let fm = FileManager.default
let execURL = (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0]))
    .resolvingSymlinksInPath()
let execDir = execURL.deletingLastPathComponent()
let factoryDylib = execDir.appendingPathComponent("libEjaUI.dylib")
let factoryDigest = sha256(at: factoryDylib) ?? ""
if !factoryDigest.isEmpty {
    setenv("EJA_FACTORY_UI_DIGEST", factoryDigest, 1)
}

let dataDir = resolveDataDir()
let uiDir = dataDir.appendingPathComponent("ui", isDirectory: true)
let evolvedDylib = uiDir.appendingPathComponent("libEjaUI.dylib")
let stampFile = uiDir.appendingPathComponent(".factory-digest")
try? fm.removeItem(at: uiDir.appendingPathComponent("EjaUI"))

let safeMode = isSafeModeRequested()
if safeMode {
    setenv("EJA_SAFE_MODE", "1", 1)
    try? fm.removeItem(at: evolvedDylib)
} else if !factoryDigest.isEmpty {
    let recorded = (try? String(contentsOf: stampFile, encoding: .utf8))?
        .trimmingCharacters(in: .whitespacesAndNewlines)
    if recorded != factoryDigest {
        try? fm.removeItem(at: evolvedDylib)
        try? fm.removeItem(at: stampFile)
    }
}

if !safeMode && fm.fileExists(atPath: evolvedDylib.path) {
    if loadAndRun(dylibURL: evolvedDylib) {
        exit(0)
    }
    try? fm.removeItem(at: evolvedDylib)
}

if !loadAndRun(dylibURL: factoryDylib) {
    fputs("Failed to load EJA UI runtime at \(factoryDylib.path)\n", stderr)
    exit(1)
}
