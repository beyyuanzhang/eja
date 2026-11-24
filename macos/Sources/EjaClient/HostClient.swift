import Foundation
import UserNotifications

public struct ProviderConfigDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String
    public var kind: String // "openai" | "anthropic"
    public var baseUrl: String
    public var apiKey: String
    public var hasApiKey: Bool
    public var models: [String]

    enum CodingKeys: String, CodingKey {
        case id, name, kind, models
        case baseUrl = "base_url"
        case apiKey = "api_key"
        case hasApiKey = "has_api_key"
    }

    public init(
        id: String,
        name: String,
        kind: String = "openai",
        baseUrl: String,
        apiKey: String = "",
        hasApiKey: Bool = false,
        models: [String] = []
    ) {
        self.id = id
        self.name = name
        self.kind = kind
        self.baseUrl = baseUrl
        self.apiKey = apiKey
        self.hasApiKey = hasApiKey
        self.models = models
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? "openai"
        baseUrl = try c.decode(String.self, forKey: .baseUrl)
        apiKey = try c.decodeIfPresent(String.self, forKey: .apiKey) ?? ""
        hasApiKey = try c.decodeIfPresent(Bool.self, forKey: .hasApiKey) ?? !apiKey.isEmpty
        models = try c.decodeIfPresent([String].self, forKey: .models) ?? []
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

public indirect enum JSONValue: Codable, Equatable, Sendable {
    case string(String)
    case number(Double)
    case bool(Bool)
    case object([String: JSONValue])
    case array([JSONValue])
    case null

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else if let o = try? c.decode([String: JSONValue].self) { self = .object(o) }
        else { self = .null }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .string(let s): try c.encode(s)
        case .number(let n): try c.encode(n)
        case .bool(let b): try c.encode(b)
        case .object(let o): try c.encode(o)
        case .array(let a): try c.encode(a)
        case .null: try c.encodeNil()
        }
    }
}

public struct CapabilityTransportDTO: Codable, Equatable, Sendable {
    public var type: String // "builtin" | "exec" | "stdio" | "http"
    public var command: String?
    public var args: [String]?
    public var env: [String: String]?
    public var description: String?
    public var schema: JSONValue?
    public var url: String?
    public var headers: [String: String]?

    public static func builtin() -> Self {
        Self(type: "builtin")
    }

    public static func exec(
        command: String,
        args: [String] = [],
        env: [String: String] = [:],
        description: String = "",
        schema: JSONValue? = nil
    ) -> Self {
        Self(
            type: "exec",
            command: command,
            args: args,
            env: env,
            description: description,
            schema: schema,
            url: nil,
            headers: nil
        )
    }

    public static func stdio(command: String, args: [String] = [], env: [String: String] = [:]) -> Self {
        Self(type: "stdio", command: command, args: args, env: env, description: nil, schema: nil, url: nil, headers: nil)
    }

    public static func http(url: String, headers: [String: String] = [:]) -> Self {
        Self(type: "http", command: nil, args: nil, env: nil, description: nil, schema: nil, url: url, headers: headers)
    }
}

public struct CapabilityConfigDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String
    public var enabled: Bool
    public var transport: CapabilityTransportDTO

    public init(id: String, name: String, enabled: Bool = true, transport: CapabilityTransportDTO) {
        self.id = id
        self.name = name
        self.enabled = enabled
        self.transport = transport
    }
}

public struct HostConfigDTO: Codable, Equatable, Sendable {
    public var providers: [ProviderConfigDTO]
    public var judge: ModelRefDTO
    public var capabilities: [CapabilityConfigDTO]
    public var windowMaxBytes: Int?
    public var maxSteps: Int?

    enum CodingKeys: String, CodingKey {
        case providers, judge, capabilities
        case windowMaxBytes = "window_max_bytes"
        case maxSteps = "max_steps"
    }

    public init(
        providers: [ProviderConfigDTO] = [],
        judge: ModelRefDTO = ModelRefDTO(),
        capabilities: [CapabilityConfigDTO] = [],
        windowMaxBytes: Int? = nil,
        maxSteps: Int? = nil
    ) {
        self.providers = providers
        self.judge = judge
        self.capabilities = capabilities
        self.windowMaxBytes = windowMaxBytes
        self.maxSteps = maxSteps
    }

}

public struct CapabilityToolSummaryDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String { capability }
    public var name: String
    public var description: String
    public var capability: String
}

public struct CapabilityStatusDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String
    public var enabled: Bool
    public var connected: Bool
    public var authRequired: Bool
    public var oauthCode: String?
    public var error: String?
    public var tools: [CapabilityToolSummaryDTO]

    enum CodingKeys: String, CodingKey {
        case id, name, enabled, connected, error, tools
        case authRequired = "auth_required"
        case oauthCode = "oauth_code"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        enabled = try c.decode(Bool.self, forKey: .enabled)
        connected = try c.decode(Bool.self, forKey: .connected)
        authRequired = try c.decodeIfPresent(Bool.self, forKey: .authRequired) ?? false
        oauthCode = try c.decodeIfPresent(String.self, forKey: .oauthCode)
        error = try c.decodeIfPresent(String.self, forKey: .error)
        tools = try c.decodeIfPresent([CapabilityToolSummaryDTO].self, forKey: .tools) ?? []
    }
}

public struct StreamItemDTO: Codable, Equatable, Identifiable, Sendable {
    public var id: Int64
    public var kind: String
    public var text: String
}

public struct ProviderTestResult: Equatable, Sendable {
    public var ok: Bool
    public var message: String
}

private struct StateEventDTO: Decodable {
    let status: String
    let statusDetail: String?
    let dataDir: String
    let configured: Bool
    let config: HostConfigDTO
    let capabilityStatuses: [CapabilityStatusDTO]
    let stream: [StreamItemDTO]
    let saveId: String?

    enum CodingKeys: String, CodingKey {
        case status, config, stream, configured
        case statusDetail = "status_detail"
        case dataDir = "data_dir"
        case capabilityStatuses = "capability_statuses"
        case saveId = "save_id"
    }
}

@MainActor
public final class HostClient: ObservableObject {
    @Published public private(set) var isReady: Bool = false
    @Published public private(set) var alreadyRunning: Bool = false
    @Published public private(set) var alreadyRunningPid: Int32?
    @Published public private(set) var status: String = "idle"
    @Published public private(set) var statusDetail: String = ""
    @Published public private(set) var streamingText: String = ""
    @Published public private(set) var dataDir: String = ""
    @Published public private(set) var isConfigured: Bool = false
    @Published public private(set) var config: HostConfigDTO = HostConfigDTO()
    @Published public private(set) var capabilityStatuses: [CapabilityStatusDTO] = []
    @Published public private(set) var stream: [StreamItemDTO] = []
    @Published public var providerTestResults: [String: ProviderTestResult] = [:]
    @Published public var errorBanner: String?

    private var process: Process?
    private var stdinHandle: FileHandle?
    private let decoder = JSONDecoder()
    private var configSaves: [String: (HostConfigDTO) -> Void] = [:]
    private var modelLists: [String: ([String]?) -> Void] = [:]

    public init(hostExecutableURL: URL? = nil, dataDir: URL? = nil) {
        if Bundle.main.bundleURL.pathExtension == "app" {
            UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
        }
        let resolvedHost = hostExecutableURL ?? Self.resolveHostExecutable()
        let resolvedDataDir = dataDir ?? ProcessInfo.processInfo.environment["EJA_DATA_DIR"]
            .flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0, isDirectory: true) }
        startHost(executableURL: resolvedHost, dataDir: resolvedDataDir)
    }

    public func shutdown(wait: Bool = true) {
        try? stdinHandle?.close()
        stdinHandle = nil
        if wait { process?.waitUntilExit() }
    }

    public func sendMessage(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        errorBanner = nil
        sendCommand(["cmd": "send", "text": trimmed])
    }

    public func saveConfig(
        providers: [ProviderConfigDTO]? = nil,
        judge: ModelRefDTO? = nil,
        capabilities: [CapabilityConfigDTO]? = nil,
        id: UUID = UUID(),
        completion: ((HostConfigDTO) -> Void)? = nil
    ) {
        struct Patch: Encodable {
            var providers: [ProviderConfigDTO]?
            var judge: ModelRefDTO?
            var capabilities: [CapabilityConfigDTO]?
        }
        let patch = Patch(providers: providers, judge: judge, capabilities: capabilities)
        guard let data = try? JSONEncoder().encode(patch),
              let fields = try? JSONSerialization.jsonObject(with: data) else {
            completion?(config)
            return
        }
        errorBanner = nil
        let id = id.uuidString
        if sendCommand(["cmd": "save_config", "config": fields, "id": id]) {
            configSaves[id] = completion
        } else {
            completion?(config)
        }
    }

    public func listModels(for provider: ProviderConfigDTO, completion: @escaping ([String]?) -> Void) {
        let id = UUID().uuidString
        guard let data = try? JSONEncoder().encode(provider),
              let fields = try? JSONSerialization.jsonObject(with: data),
              sendCommand(["cmd": "list_models", "id": id, "provider": fields]) else {
            completion(nil)
            return
        }
        modelLists[id] = completion
    }

    public func testProvider(_ provider: ProviderConfigDTO, model: String) {
        providerTestResults[provider.id] = ProviderTestResult(ok: true, message: "…")
        guard let data = try? JSONEncoder().encode(provider),
              let obj = try? JSONSerialization.jsonObject(with: data) else { return }
        sendCommand(["cmd": "test_provider", "id": provider.id, "provider": obj, "model": model])
    }

    public func reloadCapabilities() {
        errorBanner = nil
        sendCommand(["cmd": "reload_capabilities"])
    }

    public func oauthCapability(id: String) {
        errorBanner = nil
        sendCommand(["cmd": "oauth_capability", "id": id])
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

        readEvents(from: outPipe.fileHandleForReading, process: proc)
    }

    private func readEvents(from reader: FileHandle, process: Process?) {
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
                    DispatchQueue.main.async { [weak self] in
                        self?.handleEventLine(lineData)
                    }
                }
            }
            process?.waitUntilExit()
            let exitCode = process?.terminationStatus ?? 0
            DispatchQueue.main.async { [weak self] in
                self?.handleHostExited(process, status: exitCode)
            }
        }
    }

    private func handleHostExited(_ exited: Process?, status exitCode: Int32) {
        guard process === exited else { return }
        process = nil
        stdinHandle = nil
        isReady = false
        status = "idle"
        statusDetail = ""
        streamingText = ""
        let saves = configSaves
        let lists = modelLists
        configSaves.removeAll()
        modelLists.removeAll()
        for saved in saves.values { saved(config) }
        for listed in lists.values { listed(nil) }
        if !alreadyRunning && errorBanner == nil {
            errorBanner = "eja-host 已退出（退出码 \(exitCode)）"
        }
    }

    @discardableResult
    private func sendCommand(_ payload: [String: Any]) -> Bool {
        guard let handle = stdinHandle,
              let data = try? JSONSerialization.data(withJSONObject: payload) else { return false }
        var line = data
        line.append(0x0A)
        do {
            try handle.write(contentsOf: line)
            return true
        } catch {
            errorBanner = "无法向 eja-host 写入指令"
            return false
        }
    }

    func handleEventLine(_ data: Data) {
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let event = json["event"] as? String else { return }
        switch event {
        case "state":
            if let state = try? decoder.decode(StateEventDTO.self, from: data) {
                status = state.status
                statusDetail = state.statusDetail ?? ""
                streamingText = ""
                dataDir = state.dataDir
                isConfigured = state.configured
                config = state.config
                capabilityStatuses = state.capabilityStatuses
                stream = state.stream
                isReady = true
                if let id = state.saveId {
                    configSaves.removeValue(forKey: id)?(config)
                }
            }
        case "activity":
            if let detail = json["status_detail"] as? String {
                statusDetail = detail
            }
            streamingText = ""
        case "delta":
            if let chunk = json["text"] as? String, !chunk.isEmpty {
                streamingText.append(chunk)
            }
        case "already_running":
            alreadyRunning = true
            alreadyRunningPid = (json["ui_pid"] as? Int).map { Int32($0) }
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
        case "provider_models":
            if let id = json["id"] as? String, let listed = modelLists.removeValue(forKey: id) {
                if let error = json["error"] as? String { errorBanner = error }
                listed(json["models"] as? [String])
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
