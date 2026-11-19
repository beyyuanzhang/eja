import AppKit
import Foundation
import Observation

enum RuntimePhase: Equatable {
    case idle
    case starting
    case running
    case failed(String)
}

@MainActor
@Observable
final class RuntimeSupervisor {
    private(set) var phase: RuntimePhase = .idle
    private(set) var isManaged = false
    private(set) var pid: Int32?
    private(set) var baseURL: URL
    private(set) var projectDirectory: URL

    let stateDirectory: URL
    let logFile: URL

    private var process: Process?
    private var logHandle: FileHandle?
    private var expectedExit = false
    private var consecutiveFailures = 0
    private var restartTask: Task<Void, Never>?
    private let probeSession: URLSession

    init(baseURL: URL, stateDirectory: URL, projectDirectory: URL, logFile: URL) {
        self.baseURL = baseURL
        self.stateDirectory = stateDirectory
        self.projectDirectory = projectDirectory
        self.logFile = logFile

        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 1.5
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        probeSession = URLSession(configuration: configuration)
    }

    var isLocal: Bool {
        guard let host = baseURL.host?.lowercased() else { return false }
        return host == "127.0.0.1" || host == "localhost" || host == "::1"
    }

    var statusDetail: String {
        switch phase {
        case .idle:
            return "尚未启动"
        case .starting:
            return "正在启动 \(baseURL.absoluteString)"
        case .running:
            if let pid {
                return "PID \(pid) · \(baseURL.absoluteString)"
            }
            return "外部实例 · \(baseURL.absoluteString)"
        case .failed(let message):
            return message
        }
    }

    func reconfigure(baseURL: URL) async {
        guard baseURL != self.baseURL else {
            await ensureRunning()
            return
        }
        let wasManaged = isManaged
        if wasManaged {
            stop()
        }
        self.baseURL = baseURL
        phase = .idle
        await ensureRunning()
    }

    func updateProjectDirectory(_ url: URL) async {
        projectDirectory = url
        guard isManaged else { return }
        await restart()
    }

    func ensureRunning() async {
        if await probe() {
            if phase != .running {
                phase = .running
            }
            return
        }
        guard isLocal else {
            phase = .failed("连不上外部运行时 \(baseURL.absoluteString)")
            return
        }
        if case .starting = phase {
            return
        }
        await start()
    }

    func restart() async {
        stop()
        consecutiveFailures = 0
        await start()
    }

    func stop() {
        expectedExit = true
        restartTask?.cancel()
        restartTask = nil
        consecutiveFailures = 0

        guard let process else {
            phase = .idle
            return
        }
        self.process = nil
        pid = nil
        isManaged = false
        if process.isRunning {
            process.terminate()
            let deadline = Date().addingTimeInterval(5)
            while process.isRunning && Date() < deadline {
                usleep(100_000)
            }
            if process.isRunning {
                kill(process.processIdentifier, SIGKILL)
            }
        }
        closeLog()
        phase = .idle
    }

    func openLog() {
        if !FileManager.default.fileExists(atPath: logFile.path) {
            try? FileManager.default.createDirectory(
                at: logFile.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            FileManager.default.createFile(atPath: logFile.path, contents: nil)
        }
        NSWorkspace.shared.open(logFile)
    }

    private func start() async {
        guard !Task.isCancelled else {
            phase = .idle
            return
        }
        guard let executable = Self.runtimeExecutable() else {
            phase = .failed("应用里找不到 eja 运行时。")
            return
        }
        guard let port = baseURL.port else {
            phase = .failed("运行时地址缺少端口。")
            return
        }

        do {
            try FileManager.default.createDirectory(
                at: stateDirectory,
                withIntermediateDirectories: true
            )
            try FileManager.default.createDirectory(
                at: projectDirectory,
                withIntermediateDirectories: true
            )
            try FileManager.default.createDirectory(
                at: logFile.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            rotateLogIfNeeded()
            if !FileManager.default.fileExists(atPath: logFile.path) {
                FileManager.default.createFile(atPath: logFile.path, contents: nil)
            }
        } catch {
            phase = .failed("准备运行目录失败：\(error.localizedDescription)")
            return
        }

        phase = .starting
        expectedExit = false

        let child = Process()
        child.executableURL = executable
        child.currentDirectoryURL = projectDirectory
        child.arguments = [
            "serve",
            "--host", "127.0.0.1",
            "--port", String(port),
            "--dir", stateDirectory.path,
            "--project", projectDirectory.path,
        ]

        do {
            let handle = try FileHandle(forWritingTo: logFile)
            try handle.seekToEnd()
            let header = "\n[\(Self.timestamp())] eja \(child.arguments?.joined(separator: " ") ?? "")\n"
            try handle.write(contentsOf: Data(header.utf8))
            child.standardOutput = handle
            child.standardError = handle
            child.standardInput = FileHandle.nullDevice
            logHandle = handle
        } catch {
            phase = .failed("打不开运行日志：\(error.localizedDescription)")
            return
        }

        child.terminationHandler = { [weak self] exited in
            Task { @MainActor in
                self?.handleExit(exited)
            }
        }

        do {
            guard !Task.isCancelled else { return }
            try child.run()
        } catch {
            closeLog()
            phase = .failed("启动 eja 失败：\(error.localizedDescription)")
            return
        }

        process = child
        isManaged = true
        pid = child.processIdentifier

        for _ in 0 ..< 40 {
            try? await Task.sleep(for: .milliseconds(250))
            if Task.isCancelled { return }
            if await probe() {
                phase = .running
                consecutiveFailures = 0
                return
            }
            if !child.isRunning {
                break
            }
        }

        if child.isRunning {
            phase = .failed("eja 已启动，但没有响应。")
        } else if case .failed = phase {
            return
        } else {
            phase = .failed(failureSummary())
        }
    }

    private func handleExit(_ exited: Process) {
        guard process === exited else { return }
        let wasExpected = expectedExit
        process = nil
        pid = nil
        isManaged = false
        closeLog()
        if wasExpected {
            phase = .idle
            return
        }

        phase = .failed(failureSummary(status: exited.terminationStatus))
        guard consecutiveFailures < 3 else { return }
        consecutiveFailures += 1
        restartTask?.cancel()
        restartTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(2))
            guard let self, !Task.isCancelled else { return }
            await self.start()
        }
    }

    private func probe() async -> Bool {
        var request = URLRequest(url: baseURL.appendingPathComponent("api/health"))
        request.httpMethod = "GET"
        request.timeoutInterval = 1.5
        guard let (data, response) = try? await probeSession.data(for: request),
              let http = response as? HTTPURLResponse,
              (200 ..< 300).contains(http.statusCode),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              object["journal"] != nil,
              object["model"] != nil else {
            return false
        }
        return true
    }

    private func failureSummary(status: Int32? = nil) -> String {
        if let status {
            let tail = logTail()
            if !tail.isEmpty {
                return "eja 已退出（\(status)）：\(tail)"
            }
            return "eja 已退出，状态码 \(status)。"
        }
        let tail = logTail()
        return tail.isEmpty ? "eja 已退出。" : "eja 已退出：\(tail)"
    }

    private func logTail(limit: Int = 2_400) -> String {
        guard let handle = try? FileHandle(forReadingFrom: logFile) else { return "" }
        defer { try? handle.close() }
        let end = (try? handle.seekToEnd()) ?? 0
        let start = end > UInt64(limit) ? end - UInt64(limit) : 0
        try? handle.seek(toOffset: start)
        guard let data = try? handle.readToEnd(),
              let text = String(data: data, encoding: .utf8) else {
            return ""
        }
        let lines = text
            .split(separator: "\n", omittingEmptySubsequences: true)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
        return lines.suffix(3).joined(separator: " ")
    }

    private func rotateLogIfNeeded() {
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: logFile.path),
              let size = attributes[.size] as? NSNumber,
              size.intValue > 5_000_000 else {
            return
        }
        let archive = logFile.appendingPathExtension("1")
        try? FileManager.default.removeItem(at: archive)
        try? FileManager.default.moveItem(at: logFile, to: archive)
    }

    private func closeLog() {
        try? logHandle?.close()
        logHandle = nil
    }

    private static func runtimeExecutable() -> URL? {
        if let override = ProcessInfo.processInfo.environment["EJA_RUNTIME_BIN"],
           !override.isEmpty {
            let url = URL(fileURLWithPath: NSString(string: override).expandingTildeInPath)
            if FileManager.default.isExecutableFile(atPath: url.path) {
                return url
            }
        }

        let bundled = Bundle.main.bundleURL
            .appendingPathComponent("Contents/MacOS/eja-runtime")
        if FileManager.default.isExecutableFile(atPath: bundled.path) {
            return bundled
        }

        let besideBinary = Bundle.main.executableURL?
            .deletingLastPathComponent()
            .appendingPathComponent("eja")
        if let besideBinary,
           FileManager.default.isExecutableFile(atPath: besideBinary.path) {
            return besideBinary
        }
        return nil
    }

    private static func timestamp() -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.string(from: Date())
    }
}
