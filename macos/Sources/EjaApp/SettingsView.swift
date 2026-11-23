import AppKit
import EjaClient
import SwiftUI

enum SettingsSection: Hashable {
    case models
    case connectors
}

@MainActor
final class SettingsDraft: ObservableObject {
    @Published var section: SettingsSection = .models
    @Published var providers: [ProviderConfigDTO] = []
    @Published var judge = ModelRefDTO()
    @Published var connectors: [ConnectorConfigDTO] = []
    @Published var expandedProvider: String?
    @Published var expandedConnector: String?
    @Published var savedFlash = false

    private var savedProviders: [ProviderConfigDTO] = []
    private var savedJudge = ModelRefDTO()
    private var savedConnectors: [ConnectorConfigDTO] = []
    private weak var client: HostClient?
    private var saveTask: Task<Void, Never>?

    func attach(client: HostClient) {
        self.client = client
    }

    func load(from config: HostConfigDTO) {
        providers = config.providers
        judge = config.judge
        connectors = config.connectors
        savedProviders = providers
        savedJudge = judge
        savedConnectors = connectors
        if expandedProvider == nil { expandedProvider = providers.first?.id }
    }

    func syncExternalConnectors(_ latest: [ConnectorConfigDTO]) {
        guard connectors == savedConnectors else { return }
        connectors = latest
        savedConnectors = latest
        if let id = expandedConnector, !latest.contains(where: { $0.id == id }) {
            expandedConnector = latest.first?.id
        }
    }

    var providersDirty: Bool { providers != savedProviders || judge != savedJudge }
    var connectorsDirty: Bool { connectors != savedConnectors }
    var isDirty: Bool { providersDirty || connectorsDirty }

    func markSaved() {
        savedProviders = providers
        savedJudge = judge
        savedConnectors = connectors
    }

    func addProvider(_ preset: ProviderPreset) {
        var uniqueId = preset.id
        var n = 2
        while providers.contains(where: { $0.id == uniqueId }) {
            uniqueId = "\(preset.id)-\(n)"
            n += 1
        }
        providers.append(
            ProviderConfigDTO(
                id: uniqueId,
                name: preset.name,
                kind: preset.kind,
                baseUrl: preset.baseUrl,
                apiKey: "",
                models: preset.models
            )
        )
        expandedProvider = uniqueId
        if judge.providerId.isEmpty {
            judge = ModelRefDTO(providerId: uniqueId, model: preset.models.first ?? "")
        }
    }

    func removeProvider(id: String) {
        providers.removeAll { $0.id == id }
        if judge.providerId == id {
            judge = ModelRefDTO(
                providerId: providers.first?.id ?? "",
                model: providers.first?.models.first ?? ""
            )
        }
        if expandedProvider == id { expandedProvider = providers.first?.id }
    }

    func addConnector(_ kind: String) {
        var uniqueId = "mcp-\(connectors.count + 1)"
        var n = connectors.count + 2
        while connectors.contains(where: { $0.id == uniqueId }) {
            uniqueId = "mcp-\(n)"
            n += 1
        }
        let transport = kind == "http"
            ? ConnectorTransportDTO.http(url: "http://127.0.0.1:8080/mcp")
            : ConnectorTransportDTO.stdio(command: "/usr/bin/python3", args: [])
        connectors.append(ConnectorConfigDTO(id: uniqueId, name: "新连接器", enabled: true, transport: transport))
        expandedConnector = uniqueId
    }

    func removeConnector(id: String) {
        connectors.removeAll { $0.id == id }
        if expandedConnector == id { expandedConnector = connectors.first?.id }
    }

    func scheduleSave() {
        guard isDirty else { return }
        saveTask?.cancel()
        saveTask = Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 550_000_000)
            guard !Task.isCancelled else { return }
            self?.flushSave()
        }
    }

    func flushSave() {
        saveTask?.cancel()
        saveTask = nil
        guard isDirty, let client else { return }
        let providers = providersDirty ? providers : nil
        let judge = providersDirty ? judge : nil
        let connectors = connectorsDirty ? connectors : nil
        guard providers != nil || connectors != nil else { return }
        client.saveConfigPatch(providers: providers, judge: judge, connectors: connectors)
        markSaved()
        savedFlash = true
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_200_000_000)
            self?.savedFlash = false
        }
    }
}

struct ProviderPreset: Identifiable {
    let id: String
    let name: String
    let kind: String
    let baseUrl: String
    let models: [String]

    static let all: [ProviderPreset] = [
        .init(id: "openai", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", models: ["gpt-4o", "gpt-4o-mini"]),
        .init(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", models: ["claude-sonnet-4-5", "claude-haiku-4-5"]),
        .init(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", models: ["deepseek-chat", "deepseek-reasoner"]),
        .init(id: "openrouter", name: "OpenRouter", kind: "openai", baseUrl: "https://openrouter.ai/api/v1", models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]),
        .init(id: "ollama", name: "Ollama", kind: "openai", baseUrl: "http://127.0.0.1:11434/v1", models: ["qwen2.5:14b"]),
        .init(id: "custom", name: "自定义", kind: "openai", baseUrl: "", models: []),
    ]
}

struct SettingsView: View {
    @ObservedObject var client: HostClient
    @StateObject private var draft = SettingsDraft()

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Rectangle()
                .fill(EjaTheme.hairline)
                .frame(width: 1)
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: 640, height: 452)
        .background(EjaTheme.canvas)
        .tint(EjaTheme.ink)
        .onAppear {
            draft.attach(client: client)
            draft.load(from: client.config)
        }
        .onChange(of: client.config.connectors) { _, latest in
            draft.syncExternalConnectors(latest)
        }
        .onChange(of: draft.providers) { _, _ in draft.scheduleSave() }
        .onChange(of: draft.judge) { _, _ in draft.scheduleSave() }
        .onChange(of: draft.connectors) { _, _ in draft.scheduleSave() }
        .onDisappear { draft.flushSave() }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 2) {
            sideItem(.models, title: "模型")
            sideItem(.connectors, title: "连接器")
            Spacer(minLength: 0)
            Image(systemName: "checkmark")
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(EjaTheme.inkMuted)
                .opacity(draft.savedFlash ? 1 : 0)
                .padding(.leading, 10)
                .padding(.bottom, 14)
                .animation(.easeOut(duration: 0.2), value: draft.savedFlash)
        }
        .padding(.horizontal, 10)
        .padding(.top, 14)
        .frame(width: 146)
    }

    private func sideItem(_ section: SettingsSection, title: String) -> some View {
        let active = draft.section == section
        return Button {
            withAnimation(.easeOut(duration: 0.14)) { draft.section = section }
        } label: {
            Text(title)
                .font(.system(size: 12.5, weight: active ? .medium : .regular))
                .foregroundStyle(active ? EjaTheme.ink : EjaTheme.inkSecondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(active ? EjaTheme.surfaceSubtle : .clear)
                )
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    @ViewBuilder
    private var content: some View {
        switch draft.section {
        case .models:
            modelsPane
        case .connectors:
            connectorsPane
        }
    }

    // MARK: 模型

    private var modelsPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                currentModelRow
                providerList
            }
            .padding(.horizontal, 20)
            .padding(.top, 16)
            .padding(.bottom, 20)
        }
        .scrollIndicators(.never)
    }

    private var currentModelRow: some View {
        let provider = draft.providers.first { $0.id == draft.judge.providerId }
        let models = provider?.models ?? []
        let modelBinding = Binding(
            get: { draft.judge.model },
            set: { draft.judge.model = $0 }
        )
        return HStack(spacing: 8) {
            Menu {
                if draft.providers.isEmpty {
                    Text("先添加供应商")
                } else {
                    ForEach(draft.providers) { p in
                        Button(p.name) {
                            draft.judge.providerId = p.id
                            if !p.models.contains(draft.judge.model) {
                                draft.judge.model = p.models.first ?? draft.judge.model
                            }
                        }
                    }
                }
            } label: {
                HStack(spacing: 5) {
                    Text(provider?.name ?? "选择供应商")
                        .font(.system(size: 12.5))
                        .foregroundStyle(provider == nil ? EjaTheme.inkMuted : EjaTheme.ink)
                        .lineLimit(1)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 8, weight: .semibold))
                        .foregroundStyle(EjaTheme.inkMuted)
                }
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(EjaTheme.surface)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .stroke(EjaTheme.hairline, lineWidth: 1)
                )
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()

            HStack(spacing: 4) {
                TextField("", text: modelBinding, prompt: Text("模型").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)

                if !models.isEmpty {
                    Menu {
                        ForEach(models, id: \.self) { m in
                            Button(m) {
                                draft.judge.providerId = provider?.id ?? draft.judge.providerId
                                draft.judge.model = m
                            }
                        }
                    } label: {
                        Image(systemName: "chevron.down")
                            .font(.system(size: 8, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .frame(width: 14, height: 14)
                            .contentShape(Rectangle())
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                }
            }
            .padding(.horizontal, 9)
            .padding(.vertical, 6)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(EjaTheme.surface)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .stroke(EjaTheme.hairline, lineWidth: 1)
            )

            Spacer(minLength: 0)
        }
    }

    private var providerList: some View {
        VStack(spacing: 0) {
            ForEach(Array(draft.providers.enumerated()), id: \.element.id) { idx, provider in
                providerRow(index: idx)
                if idx < draft.providers.count - 1 {
                    divider.inset
                }
            }

            if !draft.providers.isEmpty {
                divider.inset
            }

            Menu {
                Section("常用") {
                    ForEach(Array(ProviderPreset.all.prefix(4))) { preset in
                        Button(preset.name) { draft.addProvider(preset) }
                    }
                }
                Section("本地与自定义") {
                    ForEach(Array(ProviderPreset.all.suffix(2))) { preset in
                        Button(preset.name) { draft.addProvider(preset) }
                    }
                }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "plus")
                        .font(.system(size: 10, weight: .medium))
                    Text("添加供应商")
                        .font(.system(size: 12))
                }
                .foregroundStyle(EjaTheme.inkMuted)
                .padding(.horizontal, 12)
                .padding(.vertical, 9)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
        }
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(EjaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .stroke(EjaTheme.hairline, lineWidth: 1)
        )
    }

    private func providerRow(index: Int) -> some View {
        let provider = draft.providers[index]
        let expanded = draft.expandedProvider == provider.id
        let isMain = draft.judge.providerId == provider.id
        let result = client.providerTestResults[provider.id]
        return VStack(spacing: 0) {
            Button {
                withAnimation(.easeOut(duration: 0.16)) {
                    draft.expandedProvider = expanded ? nil : provider.id
                }
            } label: {
                HStack(spacing: 7) {
                    Text(provider.name.isEmpty ? provider.id : provider.name)
                        .font(.system(size: 12.5))
                        .foregroundStyle(EjaTheme.ink)
                        .lineLimit(1)
                    if isMain {
                        Circle()
                            .fill(EjaTheme.ink)
                            .frame(width: 4.5, height: 4.5)
                    }
                    Spacer(minLength: 4)
                    if let result {
                        Text(result.message)
                            .font(.system(size: 10.5))
                            .foregroundStyle(result.ok ? EjaTheme.positive : EjaTheme.danger)
                            .lineLimit(1)
                            .frame(maxWidth: 120, alignment: .trailing)
                    }
                    Image(systemName: "chevron.right")
                        .font(.system(size: 8.5, weight: .semibold))
                        .foregroundStyle(EjaTheme.inkMuted)
                        .rotationEffect(.degrees(expanded ? 90 : 0))
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 9)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if expanded {
                providerEditor(index: index)
            }
        }
    }

    private func providerEditor(index: Int) -> some View {
        let provider = draft.providers[index]
        let isMain = draft.judge.providerId == provider.id
        let nameBinding = providerBinding(index, \.name, "")
        let urlBinding = providerBinding(index, \.baseUrl, "")
        let keyBinding = providerBinding(index, \.apiKey, "")
        let modelsBinding = Binding<String>(
            get: { draft.providers.indices.contains(index) ? draft.providers[index].models.joined(separator: ", ") : "" },
            set: { raw in
                guard draft.providers.indices.contains(index) else { return }
                draft.providers[index].models = raw
                    .split(separator: ",")
                    .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
                    .filter { !$0.isEmpty }
            }
        )
        return VStack(alignment: .leading, spacing: 9) {
            divider
            labeled("名称") {
                TextField("", text: nameBinding, prompt: Text("DeepSeek").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 12))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }
            labeled("接口") {
                HStack(spacing: 2) {
                    kindSegment("OpenAI", tag: "openai", index: index)
                    kindSegment("Anthropic", tag: "anthropic", index: index)
                }
                .padding(2)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(EjaTheme.canvas)
                )
                .fixedSize()
            }
            labeled("URL") {
                TextField("", text: urlBinding, prompt: Text("https://api.deepseek.com/v1").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }
            labeled("Key") {
                SecureField("", text: keyBinding, prompt: Text(provider.hasApiKey ? "已保存，留空沿用" : "sk-…").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }
            labeled("模型") {
                TextField("", text: modelsBinding, prompt: Text("deepseek-chat, deepseek-reasoner").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }

            HStack(spacing: 6) {
                Button("测试") {
                    let testModel = provider.models.first ?? draft.judge.model
                    client.testProvider(provider, model: testModel.isEmpty ? "gpt-4o" : testModel)
                }
                .buttonStyle(EjaSecondaryButtonStyle(bordered: true))
                .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)

                if !isMain {
                    Button("设为默认") {
                        draft.judge = ModelRefDTO(providerId: provider.id, model: provider.models.first ?? draft.judge.model)
                    }
                    .buttonStyle(EjaSecondaryButtonStyle())
                }

                Spacer(minLength: 0)

                Button {
                    draft.removeProvider(id: provider.id)
                } label: {
                    Image(systemName: "trash")
                        .font(.system(size: 11))
                }
                .buttonStyle(EjaIconButtonStyle(size: 24))
                .help("删除供应商")
            }
            .padding(.top, 2)
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 12)
    }

    private func kindSegment(_ label: String, tag: String, index: Int) -> some View {
        let active = draft.providers.indices.contains(index) && draft.providers[index].kind == tag
        return Button {
            guard draft.providers.indices.contains(index) else { return }
            withAnimation(.easeOut(duration: 0.14)) {
                draft.providers[index].kind = tag
            }
        } label: {
            Text(label)
                .font(.system(size: 11, weight: active ? .medium : .regular))
                .foregroundStyle(active ? EjaTheme.ink : EjaTheme.inkSecondary)
                .padding(.horizontal, 8)
                .padding(.vertical, 3.5)
                .background {
                    if active {
                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                            .fill(EjaTheme.surface)
                            .shadow(color: EjaTheme.subtleShadow, radius: 2, x: 0, y: 1)
                    }
                }
        }
        .buttonStyle(.plain)
    }

    // MARK: 连接器

    private var connectorsPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                connectorList
            }
            .padding(.horizontal, 20)
            .padding(.top, 16)
            .padding(.bottom, 20)
        }
        .scrollIndicators(.never)
    }

    private var connectorList: some View {
        VStack(spacing: 0) {
            ForEach(Array(draft.connectors.enumerated()), id: \.element.id) { idx, conn in
                connectorRow(index: idx)
                if idx < draft.connectors.count - 1 {
                    divider.inset
                }
            }

            if !draft.connectors.isEmpty {
                divider.inset
            }

            HStack(spacing: 10) {
                Menu {
                    Button("Stdio 进程") { draft.addConnector("stdio") }
                    Button("HTTP 服务") { draft.addConnector("http") }
                } label: {
                    HStack(spacing: 5) {
                        Image(systemName: "plus")
                            .font(.system(size: 10, weight: .medium))
                        Text("添加连接器")
                            .font(.system(size: 12))
                    }
                    .foregroundStyle(EjaTheme.inkMuted)
                    .contentShape(Rectangle())
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()

                Spacer(minLength: 0)

                Button {
                    client.reloadConnectors()
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 10.5, weight: .medium))
                }
                .buttonStyle(EjaIconButtonStyle(size: 22))
                .help("重载连接器")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 9)
        }
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(EjaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .stroke(EjaTheme.hairline, lineWidth: 1)
        )
    }

    private func connectorRow(index: Int) -> some View {
        let conn = draft.connectors[index]
        let status = client.connectorStatuses.first { $0.id == conn.id }
        let expanded = draft.expandedConnector == conn.id
        let dot: Color = !conn.enabled
            ? EjaTheme.inkMuted.opacity(0.5)
            : (status?.connected == true ? EjaTheme.positive : EjaTheme.warning)
        let enabledBinding = Binding(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].enabled : false },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].enabled = $0 } }
        )
        return VStack(spacing: 0) {
            HStack(spacing: 7) {
                Button {
                    withAnimation(.easeOut(duration: 0.16)) {
                        draft.expandedConnector = expanded ? nil : conn.id
                    }
                } label: {
                    HStack(spacing: 7) {
                        Circle()
                            .fill(dot)
                            .frame(width: 5, height: 5)
                        Text(conn.name.isEmpty ? conn.id : conn.name)
                            .font(.system(size: 12.5))
                            .foregroundStyle(EjaTheme.ink)
                            .lineLimit(1)
                        if let err = status?.error, !err.isEmpty {
                            Image(systemName: "exclamationmark.triangle.fill")
                                .font(.system(size: 9.5))
                                .foregroundStyle(EjaTheme.warning)
                                .help(err)
                        }
                        Spacer(minLength: 4)
                        Image(systemName: "chevron.right")
                            .font(.system(size: 8.5, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .rotationEffect(.degrees(expanded ? 90 : 0))
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                Toggle("", isOn: enabledBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.mini)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)

            if expanded {
                connectorEditor(index: index)
            }
        }
    }

    private func connectorEditor(index: Int) -> some View {
        let conn = draft.connectors[index]
        let status = client.connectorStatuses.first { $0.id == conn.id }
        let nameBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].name : "" },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].name = $0 } }
        )
        let urlBinding = transportBinding(index, \.url, "")
        let cmdBinding = transportBinding(index, \.command, "")
        let argsBinding = transportArgsBinding(index)
        let kvBinding = transportKVBinding(index)
        let isHTTP = conn.transport.type == "http"
        return VStack(alignment: .leading, spacing: 9) {
            divider
            labeled("名称") {
                TextField("", text: nameBinding, prompt: Text("连接器名称").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 12))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }

            HStack(spacing: 2) {
                transportSegment("Stdio", tag: "stdio", index: index)
                transportSegment("HTTP", tag: "http", index: index)
            }
            .padding(2)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(EjaTheme.canvas)
            )
            .fixedSize()

            if isHTTP {
                labeled("URL") {
                    TextField("", text: urlBinding, prompt: Text("http://127.0.0.1:8080/mcp").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }
            } else {
                labeled("命令") {
                    TextField("", text: cmdBinding, prompt: Text("npx / python3").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }
                labeled("参数") {
                    TextField("", text: argsBinding, prompt: Text("-y @modelcontextprotocol/server-filesystem ~/").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }
            }

            labeled(isHTTP ? "请求头" : "环境") {
                TextField(
                    "",
                    text: kvBinding,
                    prompt: Text(isHTTP ? "Authorization=Bearer …" : "API_KEY=…").foregroundStyle(EjaTheme.inkMuted),
                    axis: .vertical
                )
                .font(.system(size: 11.5, design: .monospaced))
                .foregroundStyle(EjaTheme.ink)
                .textFieldStyle(.plain)
                .lineLimit(2...4)
                .modifier(CardField())
            }

            if let err = status?.error, !err.isEmpty {
                Text(err)
                    .font(.system(size: 11))
                    .foregroundStyle(EjaTheme.danger)
                    .textSelection(.enabled)
            }

            if let tools = status?.tools, !tools.isEmpty {
                VStack(alignment: .leading, spacing: 3) {
                    ForEach(tools) { tool in
                        Text(tool.capability)
                            .font(.system(size: 10.5, design: .monospaced))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .lineLimit(1)
                    }
                }
                .padding(.top, 2)
            }

            HStack {
                Spacer(minLength: 0)
                Button {
                    draft.removeConnector(id: conn.id)
                } label: {
                    Image(systemName: "trash")
                        .font(.system(size: 11))
                }
                .buttonStyle(EjaIconButtonStyle(size: 24))
                .help("删除连接器")
            }
            .padding(.top, 2)
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 12)
    }

    private func transportSegment(_ label: String, tag: String, index: Int) -> some View {
        let active = draft.connectors.indices.contains(index) && draft.connectors[index].transport.type == tag
        return Button {
            guard draft.connectors.indices.contains(index),
                  draft.connectors[index].transport.type != tag else { return }
            withAnimation(.easeOut(duration: 0.14)) {
                draft.connectors[index].transport = tag == "http"
                    ? ConnectorTransportDTO.http(url: draft.connectors[index].transport.url ?? "http://127.0.0.1:8080/mcp")
                    : ConnectorTransportDTO.stdio(
                        command: draft.connectors[index].transport.command ?? "/usr/bin/python3",
                        args: draft.connectors[index].transport.args ?? []
                    )
            }
        } label: {
            Text(label)
                .font(.system(size: 11, weight: active ? .medium : .regular))
                .foregroundStyle(active ? EjaTheme.ink : EjaTheme.inkSecondary)
                .padding(.horizontal, 8)
                .padding(.vertical, 3.5)
                .background {
                    if active {
                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                            .fill(EjaTheme.surface)
                            .shadow(color: EjaTheme.subtleShadow, radius: 2, x: 0, y: 1)
                    }
                }
        }
        .buttonStyle(.plain)
    }

    // MARK: 组件

    private var divider: some View {
        Rectangle()
            .fill(EjaTheme.hairline)
            .frame(height: 1)
    }

    private func labeled<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(spacing: 10) {
            Text(title)
                .font(.system(size: 10))
                .foregroundStyle(EjaTheme.inkMuted)
                .frame(width: 34, alignment: .leading)
            content()
        }
    }

    private func providerBinding<T>(
        _ index: Int,
        _ keyPath: WritableKeyPath<ProviderConfigDTO, T>,
        _ fallback: T
    ) -> Binding<T> {
        Binding(
            get: { draft.providers.indices.contains(index) ? draft.providers[index][keyPath: keyPath] : fallback },
            set: { if draft.providers.indices.contains(index) { draft.providers[index][keyPath: keyPath] = $0 } }
        )
    }

    private func transportBinding<T>(
        _ index: Int,
        _ keyPath: WritableKeyPath<ConnectorTransportDTO, T?>,
        _ fallback: T
    ) -> Binding<T> {
        Binding(
            get: {
                guard draft.connectors.indices.contains(index) else { return fallback }
                return draft.connectors[index].transport[keyPath: keyPath] ?? fallback
            },
            set: {
                guard draft.connectors.indices.contains(index) else { return }
                draft.connectors[index].transport[keyPath: keyPath] = $0
            }
        )
    }

    private func transportArgsBinding(_ index: Int) -> Binding<String> {
        Binding(
            get: {
                guard draft.connectors.indices.contains(index) else { return "" }
                return (draft.connectors[index].transport.args ?? []).joined(separator: " ")
            },
            set: { raw in
                guard draft.connectors.indices.contains(index) else { return }
                draft.connectors[index].transport.args = raw
                    .split(separator: " ")
                    .map(String.init)
            }
        )
    }

    private func transportKVBinding(_ index: Int) -> Binding<String> {
        Binding(
            get: {
                guard draft.connectors.indices.contains(index) else { return "" }
                let transport = draft.connectors[index].transport
                let map = transport.type == "http" ? (transport.headers ?? [:]) : (transport.env ?? [:])
                return map.keys.sorted().map { "\($0)=\(map[$0] ?? "")" }.joined(separator: "\n")
            },
            set: { raw in
                guard draft.connectors.indices.contains(index) else { return }
                var map: [String: String] = [:]
                for line in raw.split(separator: "\n") {
                    if let sep = line.firstIndex(of: "=") {
                        let key = String(line[..<sep]).trimmingCharacters(in: .whitespaces)
                        let value = String(line[line.index(after: sep)...]).trimmingCharacters(in: .whitespaces)
                        if !key.isEmpty { map[key] = value }
                    }
                }
                if draft.connectors[index].transport.type == "http" {
                    draft.connectors[index].transport.headers = map
                } else {
                    draft.connectors[index].transport.env = map
                }
            }
        )
    }
}

/// 卡片内输入框：画布色底、细描边。
private struct CardField: ViewModifier {
    func body(content: Content) -> some View {
        content
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(EjaTheme.canvas)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .stroke(EjaTheme.hairline, lineWidth: 1)
            )
    }
}

private extension View {
    /// 列表内分隔线：左侧留出缩进。
    var inset: some View {
        padding(.leading, 12)
    }
}
