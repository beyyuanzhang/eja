import Foundation

enum AppPaths {
    private static let projectKey = "eja.projectDir"
    private static let stateKey = "eja.stateDir"

    static var supportDirectory: URL {
        let base = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).first ?? URL(fileURLWithPath: NSHomeDirectory())
        return base.appendingPathComponent("Eja", isDirectory: true)
    }

    static var projectDirectory: URL {
        override("EJA_PROJECT_DIR")
            ?? stored(projectKey)
            ?? supportDirectory.appendingPathComponent("workspace", isDirectory: true)
    }

    static var stateDirectory: URL {
        override("EJA_STATE_DIR")
            ?? stored(stateKey)
            ?? supportDirectory.appendingPathComponent("state", isDirectory: true)
    }

    static var logFile: URL {
        override("EJA_LOG_FILE")
            ?? FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("Library/Logs/Eja/runtime.log")
    }

    static func setProjectDirectory(_ url: URL) {
        UserDefaults.standard.set(url.standardizedFileURL.path, forKey: projectKey)
    }

    private static func override(_ name: String) -> URL? {
        guard let raw = ProcessInfo.processInfo.environment[name], !raw.isEmpty else {
            return nil
        }
        return URL(fileURLWithPath: NSString(string: raw).expandingTildeInPath)
    }

    private static func stored(_ key: String) -> URL? {
        guard let raw = UserDefaults.standard.string(forKey: key), !raw.isEmpty else {
            return nil
        }
        return URL(fileURLWithPath: NSString(string: raw).expandingTildeInPath)
    }
}
