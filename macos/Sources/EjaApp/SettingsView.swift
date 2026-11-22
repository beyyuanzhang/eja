import AppKit
import EjaClient
import SwiftUI

@MainActor
final class SettingsDraft: ObservableObject {
    @Published var providers: [ProviderConfigDTO] = []
    @Published var judgeProviderId: String = ""
    @Published var judgeModel: String = ""
    @Published var selectedProviderId: String?

    @Published var connectors: [ConnectorConfigDTO] = []
    @Published var selectedConnectorId: String?
    @Published var activeTab: Int = 0

    func load(from config: HostConfigDTO) {
        providers = config.providers
        judgeProviderId = config.judge.providerId
        judgeModel = config.judge.model
        selectedProviderId = providers.first?.id
        connectors = config.connectors
        selectedConnectorId = connectors.first?.id
    }

    func buildConfig() -> HostConfigDTO {
        let judge = ModelRefDTO(providerId: judgeProviderId, model: judgeModel.trimmingCharacters(in: .whitespacesAndNewlines))
        return HostConfigDTO(
            providers: providers,
            judge: judge,
            connectors: connectors
        )
    }

    func addPreset(id: String, name: String, kind: String, baseUrl: String, models: [String]) {
        var uniqueId = id
        var n = 2
        while providers.contains(where: { $0.id == uniqueId }) {
            uniqueId = "\(id)-\(n)"
            n += 1
        }
        let item = ProviderConfigDTO(id: uniqueId, name: name, kind: kind, baseUrl: baseUrl, apiKey: "", models: models)
        providers.append(item)
        selectedProviderId = uniqueId
        if judgeProviderId.isEmpty {
            judgeProviderId = uniqueId
            judgeModel = models.first ?? ""
        }
    }

    func addConnector(type: String) {
        var uniqueId = "mcp-\(connectors.count + 1)"
        var n = connectors.count + 2
        while connectors.contains(where: { $0.id == uniqueId }) {
            uniqueId = "mcp-\(n)"
            n += 1
        }
        let transport: ConnectorTransportDTO = type == "http"
            ? .http(url: "http://127.0.0.1:8080/mcp")
            : .stdio(command: "/usr/bin/python3", args: [])
        let item = ConnectorConfigDTO(id: uniqueId, name: "新连接器", enabled: true, transport: transport)
        connectors.append(item)
        selectedConnectorId = uniqueId
    }
}

struct SettingsView: View {
    @ObservedObject var client: HostClient
    @Environment(\.dismiss) private var dismiss
    @StateObject private var draft = SettingsDraft()

    var body: some View {
        VStack(spacing: 0) {
            TabView(selection: $draft.activeTab) {
                llmTab
                    .tabItem { Label("LLM 模型", systemImage: "cpu") }
                    .tag(0)
                connectorsTab
                    .tabItem { Label("连接器 (MCP)", systemImage: "cable.connector") }
                    .tag(1)
            }
            .padding(14)

            Divider()
            HStack {
                if !client.dataDir.isEmpty {
                    Button("打开数据目录") {
                        NSWorkspace.shared.open(URL(fileURLWithPath: client.dataDir, isDirectory: true))
                    }
                    .buttonStyle(.link)
                    .font(.caption)
                }
                Spacer()
                Button("取消") { dismiss() }
                Button("保存") {
                    client.saveConfig(draft.buildConfig())
                    dismiss()
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
        }
        .frame(width: 660, height: 500)
        .onAppear { draft.load(from: client.config) }
    }

    private var llmTab: some View {
        VStack(alignment: .leading, spacing: 12) {
            GroupBox("主模型") {
                HStack {
                    Picker("", selection: $draft.judgeProviderId) {
                        ForEach(draft.providers) { Text($0.name).tag($0.id) }
                    }
                    .labelsHidden()
                    .frame(width: 150)
                    TextField("模型名，如 gpt-4o / deepseek-chat", text: $draft.judgeModel)
                        .textFieldStyle(.roundedBorder)
                }
                .padding(6)
            }

            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 8) {
                    HStack {
                        Text("供应商").font(.subheadline.weight(.semibold))
                        Spacer()
                        Menu("+ 添加") {
                            Button("OpenAI") { draft.addPreset(id: "openai", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", models: ["gpt-4o", "gpt-4o-mini"]) }
                            Button("DeepSeek") { draft.addPreset(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", models: ["deepseek-chat", "deepseek-reasoner"]) }
                            Button("Anthropic") { draft.addPreset(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", models: ["claude-sonnet-4-5", "claude-haiku-4-5"]) }
                            Button("OpenRouter") { draft.addPreset(id: "openrouter", name: "OpenRouter", kind: "openai", baseUrl: "https://openrouter.ai/api/v1", models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]) }
                            Button("Ollama (本地)") { draft.addPreset(id: "ollama", name: "Ollama", kind: "openai", baseUrl: "http://127.0.0.1:11434/v1", models: ["qwen2.5:14b"]) }
                        }
                        .controlSize(.small)
                    }
                    List(selection: $draft.selectedProviderId) {
                        ForEach(draft.providers) { p in
                            VStack(alignment: .leading, spacing: 2) {
                                Text(p.name).font(.callout.weight(.medium))
                                Text(p.kind).font(.caption2).foregroundStyle(.secondary)
                            }
                            .tag(p.id)
                        }
                    }
                    .listStyle(.bordered)
                }
                .frame(width: 190)

                if let idx = draft.providers.firstIndex(where: { $0.id == draft.selectedProviderId }) {
                    providerEditor(index: idx)
                } else {
                    Text("选择或添加一个 LLM 供应商").foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
        }
    }

    private func providerEditor(index: Int) -> some View {
        let provider = draft.providers[index]
        let modelsBinding = Binding<String>(
            get: { draft.providers[index].models.joined(separator: ", ") },
            set: { raw in
                draft.providers[index].models = raw.split(separator: ",").map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            }
        )
        return Form {
            TextField("名称", text: $draft.providers[index].name)
            Picker("协议", selection: $draft.providers[index].kind) {
                Text("OpenAI 兼容").tag("openai")
                Text("Anthropic").tag("anthropic")
            }
            TextField("Base URL", text: $draft.providers[index].baseUrl)
            SecureField("API Key", text: $draft.providers[index].apiKey)
            TextField("常用模型（逗号分隔）", text: modelsBinding)
            HStack {
                Button("测试连接") {
                    let testModel = draft.providers[index].models.first ?? draft.judgeModel
                    client.testProvider(draft.providers[index], model: testModel.isEmpty ? "gpt-4o" : testModel)
                }
                .disabled(draft.providers[index].baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)
                Spacer()
                Button("设为当前主模型") {
                    draft.judgeProviderId = provider.id
                    if let first = provider.models.first { draft.judgeModel = first }
                }
                Button("删除", role: .destructive) {
                    let removedId = draft.providers[index].id
                    draft.providers.remove(at: index)
                    draft.selectedProviderId = draft.providers.first?.id
                    if draft.judgeProviderId == removedId {
                        draft.judgeProviderId = draft.providers.first?.id ?? ""
                    }
                }
            }
            if let res = client.providerTestResults[provider.id] {
                Text(res.message)
                    .font(.caption)
                    .foregroundStyle(res.ok ? .green : .red)
            }
        }
        .formStyle(.grouped)
    }

    private var connectorsTab: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("统一 MCP 连接器（外部 MCP 与 EJA 自建 MCP 共用此列表与 connectors.json）")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("重载连接器") { client.reloadConnectors() }
                    .controlSize(.small)
                Menu("+ 添加 MCP") {
                    Button("Stdio 进程 (本地 / EJA 自建)") { draft.addConnector(type: "stdio") }
                    Button("HTTP (远程 / 本地服务)") { draft.addConnector(type: "http") }
                }
                .controlSize(.small)
            }

            HStack(spacing: 12) {
                List(selection: $draft.selectedConnectorId) {
                    ForEach(draft.connectors, id: \.id) { (conn: ConnectorConfigDTO) in
                        connectorRow(conn)
                            .tag(conn.id)
                    }
                }
                .listStyle(.bordered)
                .frame(width: 200)

                if let idx = draft.connectors.firstIndex(where: { $0.id == draft.selectedConnectorId }) {
                    connectorEditor(index: idx)
                } else {
                    Text("点击右上角“+ 添加 MCP”接入外部 MCP，或让 EJA 写好 MCP 后自动写入 connectors.json。")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
        }
    }

    private func connectorRow(_ conn: ConnectorConfigDTO) -> some View {
        let st = client.connectorStatuses.first(where: { $0.id == conn.id })
        let dotColor: Color = !conn.enabled ? .secondary : (st?.connected == true ? .green : .orange)
        return HStack(spacing: 8) {
            Circle()
                .fill(dotColor)
                .frame(width: 7, height: 7)
            VStack(alignment: .leading, spacing: 2) {
                Text(conn.name).font(.callout.weight(.medium))
                Text("\(conn.transport.type) · \(st?.tools.count ?? 0) 工具")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func connectorEditor(index: Int) -> some View {
        let conn = draft.connectors[index]
        let status = client.connectorStatuses.first(where: { $0.id == conn.id })
        let cmdBinding = Binding<String>(
            get: { draft.connectors[index].transport.command ?? "" },
            set: { draft.connectors[index].transport.command = $0 }
        )
        let argsBinding = Binding<String>(
            get: { (draft.connectors[index].transport.args ?? []).joined(separator: " ") },
            set: { raw in draft.connectors[index].transport.args = raw.split(separator: " ").map(String.init) }
        )
        let urlBinding = Binding<String>(
            get: { draft.connectors[index].transport.url ?? "" },
            set: { draft.connectors[index].transport.url = $0 }
        )
        let kvBinding = Binding<String>(
            get: {
                let map = conn.transport.type == "http" ? (draft.connectors[index].transport.headers ?? [:]) : (draft.connectors[index].transport.env ?? [:])
                return map.keys.sorted().map { "\($0)=\(map[$0] ?? "")" }.joined(separator: "\n")
            },
            set: { raw in
                var map: [String: String] = [:]
                for line in raw.split(separator: "\n") {
                    if let sep = line.firstIndex(of: "=") {
                        let k = String(line[..<sep]).trimmingCharacters(in: .whitespaces)
                        let v = String(line[line.index(after: sep)...]).trimmingCharacters(in: .whitespaces)
                        if !k.isEmpty { map[k] = v }
                    }
                }
                if draft.connectors[index].transport.type == "http" {
                    draft.connectors[index].transport.headers = map
                } else {
                    draft.connectors[index].transport.env = map
                }
            }
        )
        return Form {
            TextField("ID", text: $draft.connectors[index].id)
            TextField("名称", text: $draft.connectors[index].name)
            Toggle("启用", isOn: $draft.connectors[index].enabled)
            if conn.transport.type == "http" {
                TextField("MCP URL", text: urlBinding)
                TextField("请求头 (KEY=VAL，每行一条)", text: kvBinding, axis: .vertical).lineLimit(2...4)
            } else {
                TextField("命令 (如 npx / python3)", text: cmdBinding)
                TextField("参数 (空格分隔)", text: argsBinding)
                TextField("环境变量 (KEY=VAL，每行一条)", text: kvBinding, axis: .vertical).lineLimit(2...4)
            }
            if let err = status?.error {
                Text("连接失败：\(err)").font(.caption).foregroundStyle(.red)
            }
            if let tools = status?.tools, !tools.isEmpty {
                Section("已发现工具 (\(tools.count))") {
                    ForEach(tools) { tool in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(tool.capability).font(.caption.monospaced())
                            if !tool.description.isEmpty {
                                Text(tool.description).font(.caption2).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
            Button("删除连接器", role: .destructive) {
                draft.connectors.remove(at: index)
                draft.selectedConnectorId = draft.connectors.first?.id
            }
        }
        .formStyle(.grouped)
    }
}
