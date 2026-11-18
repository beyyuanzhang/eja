import Foundation

struct SettingsView: Codable, Sendable {
    var providers: [ProviderView]
    var roles: RoleBindings
    var connectors: ConnectorsView
    var capabilities: CapabilitySettings
    var limits: Limits
    var workspaces: [String: String]
    var runtimePath: String
    var stateDir: String
    var projectDir: String
}

struct ProviderView: Codable, Sendable, Identifiable {
    var id: String
    var name: String
    var baseUrl: String
    var models: [String]
    var hasApiKey: Bool
    var local: Bool
    var headers: [KeyValueView]
}

struct KeyValueView: Codable, Sendable {
    var key: String
    var value: String
    var secret: Bool
    var hasValue: Bool
    var redacted: Bool
}

struct RoleBindings: Codable, Sendable {
    var eja: RoleBinding
    var aja: RoleBinding

    static let empty = RoleBindings(eja: .empty, aja: .empty)
}

struct RoleBinding: Codable, Sendable {
    var providerId: String
    var model: String

    static let empty = RoleBinding(providerId: "", model: "")
}

struct ConnectorsView: Codable, Sendable {
    var servers: [ConnectorView]
}

struct ConnectorView: Codable, Sendable {
    var name: String
    var mode: String
    var command: String
    var url: String
    var args: [String]
    var cwd: String
    var env: [KeyValueView]
    var headers: [KeyValueView]
    var enabled: Bool
    var timeoutMs: Int
    var tools: [String]
    var readOnly: [String]
    var witness: [String]
}

struct CapabilitySettings: Codable, Sendable, Equatable {
    var workspace: Bool
    var files: Bool
    var shell: Bool
    var git: Bool
    var mcp: Bool
    var plugins: Bool

    static let defaults = CapabilitySettings(
        workspace: true,
        files: true,
        shell: true,
        git: true,
        mcp: true,
        plugins: false
    )
}

struct Limits: Codable, Sendable {
    var slots: Int
    var turnTimeoutMs: Int
    var ajaSteps: Int
    var ajaTimeoutMs: Int

    static let fallback = Limits(slots: 1, turnTimeoutMs: 0, ajaSteps: 0, ajaTimeoutMs: 0)
}

struct KeyValueDraft: Encodable, Sendable, Identifiable, Equatable {
    var id = UUID()
    var key: String
    var value: String
    var secret: Bool
    var hasValue: Bool
    var redacted: Bool

    init(key: String = "", value: String = "", secret: Bool = false, hasValue: Bool = false, redacted: Bool = false) {
        self.key = key
        self.value = value
        self.secret = secret
        self.hasValue = hasValue
        self.redacted = redacted
    }

    init(view: KeyValueView) {
        self.init(key: view.key, value: view.value, secret: view.secret, hasValue: view.hasValue, redacted: view.redacted)
    }

    enum CodingKeys: String, CodingKey {
        case key, value, redacted
    }
}

struct ProviderDraft: Encodable, Sendable, Identifiable {
    var id: String
    var name: String
    var baseUrl: String
    var apiKey: String
    var clearApiKey: Bool
    var models: [String]
    var headers: [KeyValueDraft]
    var hasApiKey: Bool
    var local: Bool

    init(view: ProviderView) {
        id = view.id
        name = view.name
        baseUrl = view.baseUrl
        apiKey = ""
        clearApiKey = false
        models = view.models
        headers = view.headers.map(KeyValueDraft.init(view:))
        hasApiKey = view.hasApiKey
        local = view.local
    }

    init() {
        id = "p_" + (0..<8).map { _ in "0123456789abcdef".randomElement()! }.map(String.init).joined()
        name = ""
        baseUrl = ""
        apiKey = ""
        clearApiKey = false
        models = []
        headers = []
        hasApiKey = false
        local = false
    }

    enum CodingKeys: String, CodingKey {
        case id, name, baseUrl, apiKey, clearApiKey, models, headers
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(id, forKey: .id)
        try container.encode(name.trimmingCharacters(in: .whitespacesAndNewlines), forKey: .name)
        try container.encode(baseUrl.trimmingCharacters(in: .whitespacesAndNewlines), forKey: .baseUrl)
        try container.encode(models, forKey: .models)
        try container.encode(headers, forKey: .headers)
        let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        if !key.isEmpty { try container.encode(key, forKey: .apiKey) }
        if clearApiKey { try container.encode(true, forKey: .clearApiKey) }
    }
}

struct ConnectorDraft: Encodable, Sendable, Identifiable {
    var id = UUID()
    var name: String
    var mode: String
    var command: String
    var url: String
    var args: [String]
    var cwd: String
    var env: [KeyValueDraft]
    var headers: [KeyValueDraft]
    var enabled: Bool
    var timeoutMs: Int
    var tools: [String]
    var readOnly: [String]
    var witness: [String]

    init(view: ConnectorView) {
        name = view.name
        mode = view.mode
        command = view.command
        url = view.url
        args = view.args
        cwd = view.cwd
        env = view.env.map(KeyValueDraft.init(view:))
        headers = view.headers.map(KeyValueDraft.init(view:))
        enabled = view.enabled
        timeoutMs = view.timeoutMs
        tools = view.tools
        readOnly = view.readOnly
        witness = view.witness
    }

    init() {
        name = ""
        mode = "command"
        command = ""
        url = ""
        args = []
        cwd = ""
        env = []
        headers = []
        enabled = true
        timeoutMs = 30_000
        tools = []
        readOnly = []
        witness = []
    }

    enum CodingKeys: String, CodingKey {
        case name, mode, command, url, args, cwd, env, headers, enabled, timeoutMs
        case tools, readOnly, witness
    }
}

struct SettingsSaveBody: Encodable, Sendable {
    var providers: [ProviderDraft]
    var roles: RoleBindings
    var connectors: ConnectorSaveBody
    var capabilities: CapabilitySettings
    var limits: Limits
    var workspaces: [String: String]
}

struct ConnectorSaveBody: Encodable, Sendable {
    var servers: [ConnectorDraft]
}

struct DiscoverBody: Encodable, Sendable {
    var id: String?
    var baseUrl: String
    var apiKey: String?
    var headers: [KeyValueDraft]?
}

struct DiscoverResult: Codable, Sendable {
    var ok: Bool
    var models: [String]?
    var total: Int?
    var latencyMs: Int
    var error: String?
}

struct ConnectorTestResult: Codable, Sendable {
    var ok: Bool
    var tools: [ConnectorTool]?
    var count: Int?
    var latencyMs: Int
    var error: String?
    var stderr: String?
}

struct ConnectorTool: Codable, Sendable {
    var name: String
    var publicName: String
    var description: String
}
