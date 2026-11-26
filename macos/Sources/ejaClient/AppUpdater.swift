import AppKit
import CryptoKit
import Foundation

public struct AppUpdate: Equatable, Sendable {
    public let version: String
    public let tag: String
    public let notes: String
    public let releasePageURL: URL
    public let dmgURL: URL?
    public let dmgFilename: String?
    public let sha256URL: URL?

    public init(
        version: String,
        tag: String,
        notes: String,
        releasePageURL: URL,
        dmgURL: URL?,
        dmgFilename: String?,
        sha256URL: URL?
    ) {
        self.version = version
        self.tag = tag
        self.notes = notes
        self.releasePageURL = releasePageURL
        self.dmgURL = dmgURL
        self.dmgFilename = dmgFilename
        self.sha256URL = sha256URL
    }
}

public enum UpdatePhase: Equatable {
    case idle
    case checking
    case upToDate(String)
    case available(AppUpdate)
    case downloading(AppUpdate)
    case installing(AppUpdate)
    case failed(String, AppUpdate?)
}

private struct GitHubReleaseDTO: Decodable {
    struct Asset: Decodable {
        let name: String
        let browserDownloadUrl: String

        enum CodingKeys: String, CodingKey {
            case name
            case browserDownloadUrl = "browser_download_url"
        }
    }

    let tagName: String
    let htmlUrl: String
    let body: String?
    let draft: Bool?
    let prerelease: Bool?
    let assets: [Asset]

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case htmlUrl = "html_url"
        case body
        case draft
        case prerelease
        case assets
    }
}

@MainActor
public final class AppUpdater: ObservableObject {
    public static let defaultRepository = "beyyuanzhang/eja"

    @Published public private(set) var phase: UpdatePhase = .idle
    @Published public var dismissedVersion: String?
    @Published public var panelOpen: Bool = false

    public let currentVersion: String
    public let repository: String
    private var checkTask: Task<Void, Never>?
    private var installTask: Task<Void, Never>?

    public init(
        currentVersion: String? = nil,
        repository: String? = nil
    ) {
        self.currentVersion = currentVersion
            ?? (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String)
            ?? "0.0.0"
        let envRepo = ProcessInfo.processInfo.environment["eja_UPDATE_REPO"]?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        self.repository = repository
            ?? (envRepo?.isEmpty == false ? envRepo! : Self.defaultRepository)
    }

    public var availableUpdate: AppUpdate? {
        switch phase {
        case .available(let update), .downloading(let update), .installing(let update):
            return update
        case .failed(_, let update):
            return update
        case .idle, .checking, .upToDate:
            return nil
        }
    }

    public var shouldShowBadge: Bool {
        guard let update = availableUpdate else { return false }
        if case .downloading = phase { return true }
        if case .installing = phase { return true }
        return dismissedVersion != update.version
    }

    public var isBusy: Bool {
        switch phase {
        case .checking, .downloading, .installing:
            return true
        default:
            return false
        }
    }

    public func checkOnLaunchIfBundled() {
        guard Bundle.main.bundleURL.pathExtension == "app" else { return }
        checkForUpdates(userInitiated: false)
    }

    public func checkForUpdates(userInitiated: Bool) {
        if case .downloading = phase { return }
        if case .installing = phase { return }
        checkTask?.cancel()
        if userInitiated {
            panelOpen = true
            dismissedVersion = nil
        }
        phase = .checking
        let repo = repository
        let current = currentVersion
        checkTask = Task {
            do {
                let update = try await Self.fetchLatestUpdate(repository: repo, currentVersion: current)
                guard !Task.isCancelled else { return }
                if let update {
                    phase = .available(update)
                } else if userInitiated {
                    phase = .upToDate(current)
                } else {
                    phase = .idle
                }
            } catch {
                guard !Task.isCancelled else { return }
                if userInitiated {
                    phase = .failed("检查更新失败：\(error.localizedDescription)", nil)
                } else {
                    phase = .idle
                }
            }
        }
    }

    public func dismissCurrentUpdate() {
        if let update = availableUpdate {
            dismissedVersion = update.version
        }
        panelOpen = false
    }

    public func openReleasePage(_ update: AppUpdate) {
        NSWorkspace.shared.open(update.releasePageURL)
    }

    public func installUpdate(
        _ update: AppUpdate,
        dataDir: String,
        baselineId: Int64,
        shutdown: @escaping () -> Void
    ) {
        guard !isBusy else { return }
        guard let dmgURL = update.dmgURL,
              Bundle.main.bundleURL.pathExtension == "app" else {
            openReleasePage(update)
            return
        }
        let appURL = Bundle.main.bundleURL.standardizedFileURL
        let parentDir = appURL.deletingLastPathComponent()
        guard FileManager.default.isWritableFile(atPath: parentDir.path) else {
            phase = .failed("当前应用目录不可写（\(parentDir.path)），请前往 GitHub 手动下载替换。", update)
            return
        }

        installTask?.cancel()
        phase = .downloading(update)
        installTask = Task { [weak self] in
            do {
                let stagedApp = try await Self.downloadAndStageApp(
                    update: update,
                    dmgURL: dmgURL,
                    parentDir: parentDir,
                    onInstalling: { @MainActor [weak self] in
                        self?.phase = .installing(update)
                    }
                )
                guard !Task.isCancelled else {
                    try? FileManager.default.removeItem(at: stagedApp)
                    return
                }
                if !dataDir.isEmpty, baselineId >= 0 {
                    let dirURL = URL(fileURLWithPath: dataDir, isDirectory: true)
                    try? "\(baselineId)".write(
                        to: UiRuntime.reloadBaselineFile(in: dirURL),
                        atomically: true,
                        encoding: .utf8
                    )
                }
                try Self.launchSwapAndRelaunch(
                    stagedApp: stagedApp,
                    targetApp: appURL,
                    dataDir: dataDir
                )
                shutdown()
                exit(0)
            } catch {
                guard !Task.isCancelled else { return }
                self?.phase = .failed("更新失败：\(error.localizedDescription)", update)
            }
        }
    }

    // MARK: - Pure helpers (unit-tested)

    public nonisolated static func normalizeVersion(_ raw: String) -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("v") || trimmed.hasPrefix("V") {
            return String(trimmed.dropFirst())
        }
        return trimmed
    }

    public nonisolated static func isNewerVersion(_ candidate: String, than current: String) -> Bool {
        let left = splitVersion(normalizeVersion(candidate))
        let right = splitVersion(normalizeVersion(current))
        let count = max(left.core.count, right.core.count)
        for index in 0..<count {
            let lhs = index < left.core.count ? left.core[index] : 0
            let rhs = index < right.core.count ? right.core[index] : 0
            if lhs != rhs { return lhs > rhs }
        }
        switch (left.prerelease, right.prerelease) {
        case (nil, nil):
            return false
        case (nil, .some):
            return true
        case (.some, nil):
            return false
        case (.some(let lPre), .some(let rPre)):
            return lPre.compare(rPre, options: .numeric) == .orderedDescending
        }
    }

    private nonisolated static func splitVersion(_ version: String) -> (core: [Int], prerelease: String?) {
        let parts = version.split(separator: "-", maxSplits: 1, omittingEmptySubsequences: false)
        let core = parts[0].split(separator: ".").map { Int($0) ?? 0 }
        let prerelease = parts.count > 1 ? String(parts[1]) : nil
        return (core, prerelease)
    }

    public nonisolated static func parseReleaseUpdate(from data: Data, currentVersion: String) throws -> AppUpdate? {
        let release = try JSONDecoder().decode(GitHubReleaseDTO.self, from: data)
        if release.draft == true || release.prerelease == true {
            return nil
        }
        let version = normalizeVersion(release.tagName)
        guard isNewerVersion(version, than: currentVersion),
              let pageURL = URL(string: release.htmlUrl) else {
            return nil
        }
        let dmgAsset = release.assets.first { $0.name.lowercased().hasSuffix(".dmg") }
        let shaAsset = release.assets.first { $0.name.caseInsensitiveCompare("SHA256SUMS.txt") == .orderedSame }
        return AppUpdate(
            version: version,
            tag: release.tagName,
            notes: (release.body ?? "").trimmingCharacters(in: .whitespacesAndNewlines),
            releasePageURL: pageURL,
            dmgURL: dmgAsset.flatMap { URL(string: $0.browserDownloadUrl) },
            dmgFilename: dmgAsset?.name,
            sha256URL: shaAsset.flatMap { URL(string: $0.browserDownloadUrl) }
        )
    }

    public nonisolated static func parseExpectedSHA256(from text: String, filename: String) -> String? {
        for line in text.split(whereSeparator: \.isNewline) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            guard !trimmed.isEmpty else { continue }
            let parts = trimmed.split(maxSplits: 1, whereSeparator: \.isWhitespace)
            guard parts.count == 2 else { continue }
            let hash = String(parts[0]).lowercased()
            var name = String(parts[1]).trimmingCharacters(in: .whitespaces)
            if name.hasPrefix("*") { name.removeFirst() }
            if name == filename, hash.count == 64 {
                return hash
            }
        }
        return nil
    }

    // MARK: - Network & DMG install helpers

    private nonisolated static func fetchLatestUpdate(repository: String, currentVersion: String) async throws -> AppUpdate? {
        guard let url = URL(string: "https://api.github.com/repos/\(repository)/releases/latest") else {
            throw UpdaterError.message("无效的仓库地址：\(repository)")
        }
        var request = URLRequest(url: url)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("eja/\(currentVersion)", forHTTPHeaderField: "User-Agent")
        request.timeoutInterval = 15

        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw UpdaterError.message("网络响应异常")
        }
        if http.statusCode == 404 {
            return nil
        }
        guard (200..<300).contains(http.statusCode) else {
            throw UpdaterError.message("GitHub 返回状态码 \(http.statusCode)")
        }
        return try parseReleaseUpdate(from: data, currentVersion: currentVersion)
    }

    private nonisolated static func downloadAndStageApp(
        update: AppUpdate,
        dmgURL: URL,
        parentDir: URL,
        onInstalling: @escaping @MainActor () -> Void
    ) async throws -> URL {
        let fm = FileManager.default
        let workDir = fm.temporaryDirectory.appendingPathComponent("eja-update-\(UUID().uuidString)", isDirectory: true)
        try fm.createDirectory(at: workDir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: workDir) }

        let (downloadedTmp, dmgResponse) = try await URLSession.shared.download(from: dmgURL)
        if let http = dmgResponse as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw UpdaterError.message("下载安装包失败（HTTP \(http.statusCode)）")
        }
        let dmgFile = workDir.appendingPathComponent(update.dmgFilename ?? "eja.dmg")
        try fm.moveItem(at: downloadedTmp, to: dmgFile)

        await onInstalling()

        if let shaURL = update.sha256URL, let dmgFilename = update.dmgFilename {
            let (shaData, shaResp) = try await URLSession.shared.data(from: shaURL)
            if let http = shaResp as? HTTPURLResponse, (200..<300).contains(http.statusCode),
               let shaText = String(data: shaData, encoding: .utf8),
               let expected = parseExpectedSHA256(from: shaText, filename: dmgFilename) {
                let fileData = try Data(contentsOf: dmgFile, options: .mappedIfSafe)
                let actual = SHA256.hash(data: fileData).map { String(format: "%02x", $0) }.joined()
                guard actual == expected else {
                    throw UpdaterError.message("安装包 SHA-256 校验不匹配")
                }
            }
        }

        let mountPoint = workDir.appendingPathComponent("mount", isDirectory: true)
        try fm.createDirectory(at: mountPoint, withIntermediateDirectories: true)
        try runTool("/usr/bin/hdiutil", ["attach", dmgFile.path, "-nobrowse", "-readonly", "-mountpoint", mountPoint.path, "-quiet"])
        defer {
            _ = try? runTool("/usr/bin/hdiutil", ["detach", mountPoint.path, "-quiet", "-force"])
        }

        let mountedApp = mountPoint.appendingPathComponent("eja.app", isDirectory: true)
        guard fm.fileExists(atPath: mountedApp.path) else {
            throw UpdaterError.message("安装包中未找到 eja.app")
        }

        let stagedApp = parentDir.appendingPathComponent(".eja-update-\(UUID().uuidString).app", isDirectory: true)
        do {
            try runTool("/usr/bin/ditto", [mountedApp.path, stagedApp.path])
            _ = try? runTool("/usr/bin/xattr", ["-dr", "com.apple.quarantine", stagedApp.path])
            return stagedApp
        } catch {
            try? fm.removeItem(at: stagedApp)
            throw error
        }
    }

    private static func launchSwapAndRelaunch(
        stagedApp: URL,
        targetApp: URL,
        dataDir: String
    ) throws {
        let pid = ProcessInfo.processInfo.processIdentifier
        let backupApp = targetApp.deletingLastPathComponent()
            .appendingPathComponent(".eja-prev-\(UUID().uuidString).app", isDirectory: true)
        let script = """
        PID="$1"
        STAGED="$2"
        TARGET="$3"
        BACKUP="$4"
        DATA_DIR="$5"
        while kill -0 "$PID" 2>/dev/null; do sleep 0.05; done
        if mv "$TARGET" "$BACKUP"; then
          if mv "$STAGED" "$TARGET"; then
            rm -rf "$BACKUP"
          else
            mv "$BACKUP" "$TARGET"
            rm -rf "$STAGED"
            exit 1
          fi
        else
          rm -rf "$STAGED"
          exit 1
        fi
        if [ -n "$DATA_DIR" ]; then
          exec /usr/bin/open -n "$TARGET" --args --data-dir "$DATA_DIR"
        else
          exec /usr/bin/open -n "$TARGET"
        fi
        """
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", script, "--", "\(pid)", stagedApp.path, targetApp.path, backupApp.path, dataDir]
        try process.run()
    }

    @discardableResult
    private nonisolated static func runTool(_ executable: String, _ arguments: [String]) throws -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        try process.run()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        let output = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if process.terminationStatus != 0 {
            throw UpdaterError.message(output.isEmpty ? "\(executable) 退出码 \(process.terminationStatus)" : output)
        }
        return output
    }
}

private enum UpdaterError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        switch self {
        case .message(let text): return text
        }
    }
}
