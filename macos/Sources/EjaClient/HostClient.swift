import Foundation
import UserNotifications

public struct ProviderConfigDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String
    public var kind: String // "openai" | "anthropic"
    public var baseUrl: String
    public var apiKey: String
    public var models: [String]

    enum CodingKeys: String, CodingKey {
        case id, name, kind, models
        case baseUrl = "base_url"
        case apiKey = "api_key"
    }

    public init(id: String, name: String, kind: String = "openai", baseUrl: String, apiKey: String = "", models: [String] = []) {
        self.id = id
        self.name = name
        self.kind = kind
        self.baseUrl = baseUrl
        self.apiKey = apiKey
        self.models = models
    }
}

public struct ModelRefDTO: Codable, Equatable, Sendable {
    public var providerId: String
    public var model: String

    enum CodingKeys: String, CodingKey {
        case providerId = "provider_id"
        case model
    }

    public init(providerId: String = "", model: String = "") {
        self.providerId = providerId
        self.model = model
    }
}

public struct ConnectorTransportDTO: Codable, Equatable, Sendable {
    public var type: String // "stdio" | "http"
    public var command: String?
    public var args: [String]?
    public var env: [String: String]?
    public var url: String?
    public var headers: [String: String]?

    public static func stdio(command: String, args: [String] = [], env: [String: String] = [:]) -> Self {
        Self(type: "stdio", command: command, args: args, env: env, url: nil, headers: nil)
    }

    public static func http(url: String, headers: [String: String] = [:]) -> Self {
        Self(type: "http", command: nil, args: nil, env: nil, url: url, headers: headers)
    }
}

public struct ConnectorConfigDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String
    public var enabled: Bool
    public var transport: ConnectorTransportDTO

    public init(id: String, name: String, enabled: Bool = true, transport: ConnectorTransportDTO) {
        self.id = id
        self.name = name
        self.enabled = enabled
        self.transport = transport
    }
}

public struct HostConfigDTO: Codable, Equatable, Sendable {
    public var providers: [ProviderConfigDTO]
    public var judge: ModelRefDTO
    public var connectors: [ConnectorConfigDTO]
    public var windowMaxBytes: Int?
    public var maxSteps: Int?

    enum CodingKeys: String, CodingKey {
        case providers, judge, connectors
        case windowMaxBytes = "window_max_bytes"
        case maxSteps = "max_steps"
    }

    public init(
        providers: [ProviderConfigDTO] = [],
        judge: ModelRefDTO = ModelRefDTO(),
        connectors: [ConnectorConfigDTO] = [],
        windowMaxBytes: Int? = nil,
        maxSteps: Int? = nil
    ) {
        self.providers = providers
        self.judge = judge
        self.connectors = connectors
        self.windowMaxBytes = windowMaxBytes
        self.maxSteps = maxSteps
    }
}

public struct McpToolSummaryDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String { capability }
    public var name: String
    public var description: String
    public var capability: String
}

public struct ConnectorStatusDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String
    public var enabled: Bool
    public var connected: Bool
    public var error: String?
    public var tools: [McpToolSummaryDTO]
}

public struct StreamItemDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: Int64
    public var kind: String
    public var text: String
    public var detail: String?
    public var proactive: Bool?
}

public struct ConcernDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: Int64
    public var text: String
}

public struct WakeAlarmDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var fireAtUnix: Int64
    public var fireAtDisplay: String
    public var reason: String

    enum CodingKeys: String, CodingKey {
        case id, reason
        case fireAtUnix = "fire_at_unix"
        case fireAtDisplay = "fire_at_display"
    }
}

public struct ProviderTestResult: Equatable, Sendable {
    public var ok: Bool
    public var message: String
}

private struct StateEventDTO: Decodable {
    let status: String
    let statusDetail: String
    let dataDir: String
    let configured: Bool
    let config: HostConfigDTO
    let connectorStatuses: [ConnectorStatusDTO]
    let stream: [StreamItemDTO]
    let concerns: [ConcernDTO]
    let wakes: [WakeAlarmDTO]

    enum CodingKeys: String, CodingKey {
        case status, config, stream, concerns, wakes, configured
        case statusDetail = "status_detail"
        case dataDir = "data_dir"
        case connectorStatuses = "connector_statuses"
    }
}

@MainActor
public final class HostClient: ObservableObject {
    @Published public private(set) var isReady: Bool = false
    @Published public private(set) var status: String = "idle"
    @Published public private(set) var statusDetail: String = "启动中…"
    @Published public private(set) var dataDir: String = ""
    @Published public private(set) var isConfigured: Bool = false
    @Published public private(set) var config: HostConfigDTO = HostConfigDTO()
    @Published public private(set) var connectorStatuses: [ConnectorStatusDTO] = []
    @Published public private(set) var stream: [StreamItemDTO] = []
    @Published public private(set) var concerns: [ConcernDTO] = []
    @Published public private(set) var wakes: [WakeAlarmDTO] = []
    @Published public var providerTestResults: [String: ProviderTestResult] = [:]
    @Published public var errorBanner: String?
    public private(set) var hostExecutableURL: URL?

    private var process: Process?
    private var stdinHandle: FileHandle?
    private let decoder = JSONDecoder()
    private let configuredDataDir: URL?

    public var connectedToolCount: Int {
        connectorStatuses.filter(\.connected).reduce(0) { $0 + $1.tools.count }
    }

    public init(hostExecutableURL: URL? = nil, dataDir: URL? = nil) {
        if Bundle.main.bundleURL.pathExtension == "app" {
            UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
        }
        let resolvedHost = hostExecutableURL ?? Self.resolveHostExecutable()
        let resolvedDataDir = dataDir ?? ProcessInfo.processInfo.environment["EJA_DATA_DIR"]
            .flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0, isDirectory: true) }
        self.hostExecutableURL = resolvedHost
        self.configuredDataDir = resolvedDataDir
        startHost(executableURL: resolvedHost, dataDir: resolvedDataDir)
    }

    deinit {
        process?.terminate()
    }

    public func shutdown() {
        try? stdinHandle?.close()
        stdinHandle = nil
        if let proc = process {
            proc.terminate()
            proc.waitUntilExit()
        }
        process = nil
    }

    public func restart() {
        shutdown()
        startHost(executableURL: hostExecutableURL, dataDir: configuredDataDir)
    }

    public func sendMessage(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        errorBanner = nil
        sendCommand(["cmd": "send", "text": trimmed])
    }

    public func saveConfig(_ updated: HostConfigDTO) {
        errorBanner = nil
        guard let data = try? JSONEncoder().encode(updated),
              let obj = try? JSONSerialization.jsonObject(with: data) else { return }
        sendCommand(["cmd": "save_config", "config": obj])
    }

    public func testProvider(_ provider: ProviderConfigDTO, model: String) {
        providerTestResults[provider.id] = ProviderTestResult(ok: true, message: "测试中…")
        guard let data = try? JSONEncoder().encode(provider),
              let obj = try? JSONSerialization.jsonObject(with: data) else { return }
        sendCommand(["cmd": "test_provider", "id": provider.id, "provider": obj, "model": model])
    }

    public func reloadConnectors() {
        errorBanner = nil
        sendCommand(["cmd": "reload_connectors"])
    }

    private func startHost(executableURL: URL?, dataDir: URL?) {
        guard let executableURL else {
            errorBanner = "找不到 eja-host，请先运行 ./macos/scripts/build-app.sh"
            return
        }
        let proc = Process()
        proc.executableURL = executableURL
        if let dataDir {
            proc.arguments = ["--data-dir", dataDir.path]
        }
        let inPipe = Pipe()
        let outPipe = Pipe()
        proc.standardInput = inPipe
        proc.standardOutput = outPipe
        proc.standardError = FileHandle.nullDevice
        do {
            try proc.run()
        } catch {
            errorBanner = "启动 eja-host 失败：\(error.localizedDescription)"
            return
        }
        process = proc
        stdinHandle = inPipe.fileHandleForWriting

        let reader = outPipe.fileHandleForReading
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var buffer = Data()
            while true {
                let chunk = reader.availableData
                if chunk.isEmpty { break }
                buffer.append(chunk)
                while let range = buffer.range(of: Data([0x0A])) {
                    let lineData = buffer.subdata(in: buffer.startIndex..<range.lowerBound)
                    buffer.removeSubrange(buffer.startIndex...range.lowerBound)
                    if lineData.isEmpty { continue }
                    Task { @MainActor [weak self] in
                        self?.handleEventLine(lineData)
                    }
                }
            }
        }
    }

    private func sendCommand(_ payload: [String: Any]) {
        guard let handle = stdinHandle,
              let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        var line = data
        line.append(0x0A)
        try? handle.write(contentsOf: line)
    }

    private func handleEventLine(_ data: Data) {
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let event = json["event"] as? String else { return }
        switch event {
        case "state":
            if let state = try? decoder.decode(StateEventDTO.self, from: data) {
                isReady = true
                status = state.status
                statusDetail = state.statusDetail
                dataDir = state.dataDir
                isConfigured = state.configured
                config = state.config
                connectorStatuses = state.connectorStatuses
                stream = state.stream
                concerns = state.concerns
                wakes = state.wakes
            }
        case "error":
            if let msg = json["message"] as? String {
                errorBanner = msg
            }
        case "provider_test":
            if let id = json["id"] as? String,
               let ok = json["ok"] as? Bool,
               let msg = json["message"] as? String {
                providerTestResults[id] = ProviderTestResult(ok: ok, message: msg)
            }
        case "notify":
            if Bundle.main.bundleURL.pathExtension == "app",
               let title = json["title"] as? String,
               let body = json["body"] as? String {
                let content = UNMutableNotificationContent()
                content.title = title
                content.body = body
                content.sound = .default
                let req = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
                UNUserNotificationCenter.current().add(req)
            }
        default:
            break
        }
    }

    public static func resolveHostExecutable() -> URL? {
        if let envPath = ProcessInfo.processInfo.environment["EJA_HOST_BIN"] {
            let url = URL(fileURLWithPath: envPath)
            if FileManager.default.isExecutableFile(atPath: url.path) { return url }
        }
        if let execDir = Bundle.main.executableURL?.deletingLastPathComponent() {
            let sibling = execDir.appendingPathComponent("eja-host")
            if FileManager.default.isExecutableFile(atPath: sibling.path) { return sibling }
        }
        let cwd = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        for rel in ["target/release/eja-host", "../target/release/eja-host", "target/debug/eja-host", "../target/debug/eja-host"] {
            let candidate = cwd.appendingPathComponent(rel).standardized
            if FileManager.default.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }
}
