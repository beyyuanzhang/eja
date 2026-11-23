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
    @Published var fetchingProviders: Set<String> = []
    @Published var addingModelProvider: String?
    @Published var newModelInput: String = ""
    @Published var rawCommandLines: [String: String] = [:]

    private var savedProviders: [ProviderConfigDTO] = []
    private var savedJudge = ModelRefDTO()
    private var savedConnectors: [ConnectorConfigDTO] = []
    private weak var client: HostClient?
    private var saveTask: Task<Void, Never>?
    private var hasAutoSynced = false

    func attach(client: HostClient) {
        self.client = client
    }

    func load(from config: HostConfigDTO) {
        providers = config.providers
        judge = config.judge
        connectors = config.connectors
        for conn in connectors where rawCommandLines[conn.id] == nil {
            rawCommandLines[conn.id] = Self.formatCommandLine(conn.transport)
        }
        savedProviders = providers
        savedJudge = judge
        savedConnectors = connectors

        if healJudgeModelIfNeeded() {
            scheduleSave()
        }
        if let first = providers.first,
           providers.count == 1,
           first.models.isEmpty || (first.baseUrl.isEmpty && !first.hasApiKey) {
            expandedProvider = first.id
        }
        if !hasAutoSynced {
            hasAutoSynced = true
            for provider in providers where Self.isLocalEndpoint(provider.baseUrl) {
                fetchModels(for: provider.id)
            }
        }
    }

    func syncExternalConnectors(_ latest: [ConnectorConfigDTO]) {
        guard connectors == savedConnectors else { return }
        connectors = latest
        savedConnectors = latest
        for conn in latest {
            rawCommandLines[conn.id] = Self.formatCommandLine(conn.transport)
        }
        if let id = expandedConnector, !latest.contains(where: { $0.id == id }) {
            expandedConnector = nil
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

    @discardableResult
    func healJudgeModelIfNeeded() -> Bool {
        guard !providers.isEmpty else { return false }
        var changed = false
        if judge.providerId.isEmpty || !providers.contains(where: { $0.id == judge.providerId }) {
            if let first = providers.first {
                judge.providerId = first.id
                if judge.model.isEmpty, let firstModel = first.models.first {
                    judge.model = firstModel
                }
                changed = true
            }
        }
        guard let idx = providers.firstIndex(where: { $0.id == judge.providerId }) else {
            return changed
        }
        let models = providers[idx].models
        guard !models.isEmpty else { return changed }
        let current = judge.model.trimmingCharacters(in: .whitespacesAndNewlines)
        if current.isEmpty {
            judge.model = models[0]
            return true
        }
        if !models.contains(current) {
            if let matched = models.first(where: {
                $0.caseInsensitiveCompare(current) == .orderedSame ||
                $0.lowercased().hasSuffix("/" + current.lowercased())
            }) {
                judge.model = matched
                return true
            }
        }
        return changed
    }

    func selectModel(providerId: String, model: String) {
        let trimmed = model.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        if let idx = providers.firstIndex(where: { $0.id == providerId }),
           !providers[idx].models.contains(trimmed) {
            providers[idx].models.append(trimmed)
        }
        judge = ModelRefDTO(providerId: providerId, model: trimmed)
        flushSave()
    }

    func addModel(providerId: String, model: String) {
        let trimmed = model.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let idx = providers.firstIndex(where: { $0.id == providerId }) else {
            addingModelProvider = nil
            newModelInput = ""
            return
        }
        if !providers[idx].models.contains(trimmed) {
            providers[idx].models.append(trimmed)
        }
        judge = ModelRefDTO(providerId: providerId, model: trimmed)
        newModelInput = ""
        addingModelProvider = nil
        flushSave()
    }

    func removeModel(providerId: String, model: String) {
        guard let idx = providers.firstIndex(where: { $0.id == providerId }) else { return }
        providers[idx].models.removeAll { $0 == model }
        if judge.providerId == providerId && judge.model == model {
            judge.model = providers[idx].models.first ?? ""
        }
        scheduleSave()
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
        expandedProvider = preset.baseUrl.isEmpty ? uniqueId : nil
        if judge.providerId.isEmpty || judge.model.isEmpty, let firstModel = preset.models.first {
            judge = ModelRefDTO(providerId: uniqueId, model: firstModel)
        }
        flushSave()
        if !preset.baseUrl.isEmpty {
            fetchModels(for: uniqueId, selectFirstIfNeeded: true)
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
        if expandedProvider == id { expandedProvider = nil }
        flushSave()
    }

    func addConnector() {
        var uniqueId = "mcp-\(connectors.count + 1)"
        var n = connectors.count + 2
        while connectors.contains(where: { $0.id == uniqueId }) {
            uniqueId = "mcp-\(n)"
            n += 1
        }
        let transport = ConnectorTransportDTO.stdio(command: "npx", args: [])
        rawCommandLines[uniqueId] = ""
        connectors.append(ConnectorConfigDTO(id: uniqueId, name: "MCP", enabled: true, transport: transport))
        expandedConnector = uniqueId
    }

    func removeConnector(id: String) {
        connectors.removeAll { $0.id == id }
        rawCommandLines.removeValue(forKey: id)
        if expandedConnector == id { expandedConnector = nil }
        flushSave()
    }

    func scheduleSave() {
        guard isDirty else { return }
        saveTask?.cancel()
        saveTask = Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 350_000_000)
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
    }

    func fetchModels(for providerId: String, selectFirstIfNeeded: Bool = false) {
        guard let index = providers.firstIndex(where: { $0.id == providerId }) else { return }
        let provider = providers[index]
        let rawBase = provider.baseUrl.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !rawBase.isEmpty else { return }

        let cleanBase = rawBase.hasSuffix("/") ? String(rawBase.dropLast()) : rawBase
        let urlString = (provider.kind == "anthropic" && !cleanBase.hasSuffix("/v1"))
            ? "\(cleanBase)/v1/models"
            : "\(cleanBase)/models"
        guard let url = URL(string: urlString) else { return }

        let apiKey = resolveApiKey(for: provider)
        fetchingProviders.insert(providerId)

        Task { @MainActor [weak self] in
            defer { self?.fetchingProviders.remove(providerId) }
            var req = URLRequest(url: url)
            req.httpMethod = "GET"
            req.timeoutInterval = 8
            if !apiKey.isEmpty {
                if provider.kind == "anthropic" {
                    req.setValue(apiKey, forHTTPHeaderField: "x-api-key")
                    req.setValue("2023-06-01", forHTTPHeaderField: "anthropic-version")
                } else {
                    req.setValue("Bearer \(apiKey)", forHTTPHeaderField: "Authorization")
                }
            }

            guard let (data, response) = try? await URLSession.shared.data(for: req),
                  let http = response as? HTTPURLResponse,
                  (200..<300).contains(http.statusCode) else { return }

            let discovered = Self.parseModelIds(from: data)
            guard !discovered.isEmpty,
                  let self,
                  let idx = self.providers.firstIndex(where: { $0.id == providerId }) else { return }

            var merged: [String] = []
            for m in discovered where !merged.contains(m) {
                merged.append(m)
            }
            for existing in self.providers[idx].models where !merged.contains(existing) {
                let hasSuffixed = merged.contains { $0.lowercased().hasSuffix("/" + existing.lowercased()) }
                if !hasSuffixed {
                    merged.append(existing)
                }
            }

            if self.providers[idx].models != merged {
                self.providers[idx].models = merged
            }
            _ = self.healJudgeModelIfNeeded()
            if selectFirstIfNeeded || self.judge.providerId.isEmpty || (self.judge.providerId == providerId && self.judge.model.isEmpty) {
                if let first = merged.first {
                    self.judge = ModelRefDTO(providerId: providerId, model: first)
                }
            }
            self.flushSave()
        }
    }

    private func resolveApiKey(for provider: ProviderConfigDTO) -> String {
        let direct = provider.apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        if !direct.isEmpty { return direct }
        guard provider.hasApiKey else { return "" }
        let configURL = URL(fileURLWithPath: NSHomeDirectory())
            .appendingPathComponent("Library/Application Support/ai.eja.kernel/config.json")
        guard let data = try? Data(contentsOf: configURL),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let list = json["providers"] as? [[String: Any]],
              let item = list.first(where: { ($0["id"] as? String) == provider.id }),
              let key = item["api_key"] as? String else {
            return ""
        }
        return key.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private static func parseModelIds(from data: Data) -> [String] {
        guard let obj = try? JSONSerialization.jsonObject(with: data) else { return [] }
        var items: [[String: Any]] = []
        if let dict = obj as? [String: Any] {
            if let dataArr = dict["data"] as? [[String: Any]] {
                items = dataArr
            } else if let modelsArr = dict["models"] as? [[String: Any]] {
                items = modelsArr
            }
        } else if let arr = obj as? [[String: Any]] {
            items = arr
        }
        var ids: [String] = []
        for item in items {
            if let id = (item["id"] as? String) ?? (item["name"] as? String) {
                let trimmed = id.trimmingCharacters(in: .whitespacesAndNewlines)
                if !trimmed.isEmpty && !ids.contains(trimmed) {
                    ids.append(trimmed)
                }
            }
        }
        return ids
    }

    static func isLocalEndpoint(_ baseUrl: String) -> Bool {
        let lower = baseUrl.lowercased()
        return lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("0.0.0.0")
    }

    static func formatCommandLine(_ transport: ConnectorTransportDTO) -> String {
        let cmd = (transport.command ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let args = transport.args ?? []
        guard !cmd.isEmpty else { return args.joined(separator: " ") }
        let parts = [cmd] + args
        return parts.map { part in
            if part.contains(where: { $0.isWhitespace || $0 == "\"" }) {
                let escaped = part.replacingOccurrences(of: "\"", with: "\\\"")
                return "\"\(escaped)\""
            }
            return part
        }.joined(separator: " ")
    }

    static func parseCommandLine(_ raw: String) -> (command: String, args: [String]) {
        var tokens: [String] = []
        var current = ""
        var inSingleQuote = false
        var inDoubleQuote = false
        var escaping = false

        for ch in raw {
            if escaping {
                current.append(ch)
                escaping = false
                continue
            }
            if ch == "\\" && !inSingleQuote {
                escaping = true
                continue
            }
            if ch == "'" && !inDoubleQuote {
                inSingleQuote.toggle()
                continue
            }
            if ch == "\"" && !inSingleQuote {
                inDoubleQuote.toggle()
                continue
            }
            if ch.isWhitespace && !inSingleQuote && !inDoubleQuote {
                if !current.isEmpty {
                    tokens.append(current)
                    current = ""
                }
                continue
            }
            current.append(ch)
        }
        if !current.isEmpty {
            tokens.append(current)
        }
        guard let first = tokens.first else { return ("", []) }
        return (first, Array(tokens.dropFirst()))
    }
}

struct ProviderPreset: Identifiable {
    let id: String
    let name: String
    let kind: String
    let baseUrl: String
    let models: [String]

    static func detectMagpieBaseUrl() -> String {
        let gatewayFile = URL(fileURLWithPath: NSHomeDirectory())
            .appendingPathComponent(".config/magpie/gateway.json")
        if let data = try? Data(contentsOf: gatewayFile),
           let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
           let addr = json["addr"] as? String,
           !addr.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            let clean = addr.trimmingCharacters(in: .whitespacesAndNewlines)
            if clean.hasPrefix("http://") || clean.hasPrefix("https://") {
                return clean.hasSuffix("/v1") ? clean : "\(clean)/v1"
            }
            return "http://\(clean)/v1"
        }
        return "http://127.0.0.1:3425/v1"
    }

    static let all: [ProviderPreset] = [
        .init(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", models: ["claude-sonnet-4-5", "claude-haiku-4-5"]),
        .init(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", models: ["deepseek-chat", "deepseek-reasoner"]),
        .init(id: "kimi", name: "Kimi", kind: "openai", baseUrl: "https://api.kimi.com/coding/v1", models: ["k2.5", "k2-thinking"]),
        .init(id: "magpie", name: "Magpie", kind: "openai", baseUrl: detectMagpieBaseUrl(), models: []),
        .init(id: "ollama", name: "Ollama", kind: "openai", baseUrl: "http://127.0.0.1:11434/v1", models: []),
        .init(id: "openai", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", models: ["gpt-4o", "gpt-4o-mini"]),
        .init(id: "openrouter", name: "OpenRouter", kind: "openai", baseUrl: "https://openrouter.ai/api/v1", models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]),
        .init(id: "qwen", name: "Qwen", kind: "openai", baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1", models: ["qwen-max", "qwen-plus"]),
        .init(id: "custom", name: "自定义", kind: "openai", baseUrl: "", models: []),
    ]
}

struct SettingsView: View {
    @ObservedObject var client: HostClient
    @StateObject private var draft = SettingsDraft()
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Rectangle()
                .fill(EjaTheme.hairline)
                .frame(width: 1)
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: 560, height: 420)
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
            Button {
                draft.flushSave()
                dismiss()
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 11, weight: .medium))
            }
            .buttonStyle(EjaIconButtonStyle(size: 24))
            .keyboardShortcut(.cancelAction)
            .padding(.leading, 4)
            .padding(.bottom, 10)
        }
        .padding(.horizontal, 10)
        .padding(.top, 14)
        .frame(width: 112)
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

    // MARK: - 模型

    private var modelsPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(Array(draft.providers.enumerated()), id: \.element.id) { idx, _ in
                    providerCard(index: idx)
                }
                addProviderMenu
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 16)
        }
        .scrollIndicators(.never)
    }

    private var addProviderMenu: some View {
        Menu {
            ForEach(ProviderPreset.all) { preset in
                Button(preset.name) { draft.addProvider(preset) }
            }
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(EjaTheme.inkMuted)
                .frame(width: 26, height: 26)
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
    }

    private func providerCard(index: Int) -> some View {
        let provider = draft.providers[index]
        let expanded = draft.expandedProvider == provider.id
        let isFetching = draft.fetchingProviders.contains(provider.id)
        let testResult = client.providerTestResults[provider.id]
        let needsInlineKey = !expanded &&
            !SettingsDraft.isLocalEndpoint(provider.baseUrl) &&
            !provider.hasApiKey &&
            provider.apiKey.trimmingCharacters(in: .whitespaces).isEmpty

        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Button {
                    withAnimation(.easeOut(duration: 0.14)) {
                        draft.expandedProvider = expanded ? nil : provider.id
                    }
                } label: {
                    HStack(spacing: 5) {
                        Text(provider.name.isEmpty ? provider.id : provider.name)
                            .font(.system(size: 12.5, weight: .medium))
                            .foregroundStyle(EjaTheme.ink)
                            .lineLimit(1)
                        Image(systemName: "chevron.right")
                            .font(.system(size: 8.5, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .rotationEffect(.degrees(expanded ? 90 : 0))
                        Spacer(minLength: 4)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                if let testResult {
                    Text(testResult.message)
                        .font(.system(size: 10.5))
                        .foregroundStyle(testResult.ok ? EjaTheme.positive : EjaTheme.danger)
                        .lineLimit(1)
                        .frame(maxWidth: 140, alignment: .trailing)
                }

                Button {
                    let active = draft.judge.providerId == provider.id ? draft.judge.model : ""
                    let model = !active.isEmpty ? active : (provider.models.first ?? "gpt-4o")
                    client.testProvider(provider, model: model)
                } label: {
                    Image(systemName: "bolt")
                        .font(.system(size: 10.5, weight: .medium))
                }
                .buttonStyle(EjaIconButtonStyle(size: 22))
                .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)

                Button {
                    draft.fetchModels(for: provider.id)
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 10.5, weight: .medium))
                        .rotationEffect(.degrees(isFetching ? 180 : 0))
                        .animation(.easeInOut(duration: 0.25), value: isFetching)
                }
                .buttonStyle(EjaIconButtonStyle(size: 22))
                .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty || isFetching)

                if expanded {
                    Button {
                        draft.removeProvider(id: provider.id)
                    } label: {
                        Image(systemName: "trash")
                            .font(.system(size: 10.5))
                    }
                    .buttonStyle(EjaIconButtonStyle(size: 22, destructive: true))
                }
            }
            .padding(.horizontal, 11)
            .padding(.top, 9)
            .padding(.bottom, 7)

            if needsInlineKey {
                SecureField("", text: providerBinding(index, \.apiKey, ""), prompt: Text("API Key").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .onSubmit {
                        draft.flushSave()
                        draft.fetchModels(for: provider.id)
                    }
                    .modifier(CardField())
                    .padding(.horizontal, 11)
                    .padding(.bottom, 7)
            }

            providerModelPills(index: index)
                .padding(.horizontal, 11)
                .padding(.bottom, 10)

            if expanded {
                providerEditor(index: index)
            }
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

    private func providerModelPills(index: Int) -> some View {
        let provider = draft.providers[index]
        let isAdding = draft.addingModelProvider == provider.id
        return FlowLayout(spacing: 5, lineSpacing: 5) {
            ForEach(provider.models, id: \.self) { model in
                let selected = draft.judge.providerId == provider.id && draft.judge.model == model
                Button {
                    draft.selectModel(providerId: provider.id, model: model)
                } label: {
                    Text(model)
                        .font(.system(size: 11.5, weight: selected ? .medium : .regular, design: .monospaced))
                        .lineLimit(1)
                        .foregroundStyle(selected ? EjaTheme.canvas : EjaTheme.ink)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .background(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .fill(selected ? EjaTheme.ink : EjaTheme.canvas)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .stroke(selected ? .clear : EjaTheme.hairline, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
                .contextMenu {
                    Button("测试") {
                        client.testProvider(provider, model: model)
                    }
                    Button("移除", role: .destructive) {
                        draft.removeModel(providerId: provider.id, model: model)
                    }
                }
            }

            if isAdding {
                TextField("", text: $draft.newModelInput, prompt: Text("模型 ID").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .frame(width: 130)
                    .onSubmit {
                        draft.addModel(providerId: provider.id, model: draft.newModelInput)
                    }
                    .onExitCommand {
                        draft.addingModelProvider = nil
                        draft.newModelInput = ""
                    }
                    .padding(.horizontal, 7)
                    .padding(.vertical, 4)
                    .background(
                        RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                            .fill(EjaTheme.canvas)
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                            .stroke(EjaTheme.hairlineStrong, lineWidth: 1)
                    )
            } else {
                Button {
                    draft.newModelInput = ""
                    draft.addingModelProvider = provider.id
                } label: {
                    Image(systemName: "plus")
                        .font(.system(size: 9.5, weight: .medium))
                        .foregroundStyle(EjaTheme.inkMuted)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 5.5)
                        .background(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .fill(EjaTheme.canvas)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .stroke(EjaTheme.hairline, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
            }
        }
    }

    private func providerEditor(index: Int) -> some View {
        let provider = draft.providers[index]
        let nameBinding = providerBinding(index, \.name, "")
        let urlBinding = providerBinding(index, \.baseUrl, "")
        let keyBinding = providerBinding(index, \.apiKey, "")

        return VStack(alignment: .leading, spacing: 6) {
            divider
            HStack(spacing: 6) {
                TextField("", text: nameBinding, prompt: Text("名称").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 12))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())

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

            TextField("", text: urlBinding, prompt: Text("URL").foregroundStyle(EjaTheme.inkMuted))
                .font(.system(size: 11.5, design: .monospaced))
                .foregroundStyle(EjaTheme.ink)
                .textFieldStyle(.plain)
                .modifier(CardField())

            SecureField("", text: keyBinding, prompt: Text(provider.hasApiKey ? "密钥已保存" : "API Key").foregroundStyle(EjaTheme.inkMuted))
                .font(.system(size: 11.5, design: .monospaced))
                .foregroundStyle(EjaTheme.ink)
                .textFieldStyle(.plain)
                .modifier(CardField())
        }
        .padding(.horizontal, 11)
        .padding(.bottom, 10)
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

    // MARK: - 连接器

    private var connectorsPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(Array(draft.connectors.enumerated()), id: \.element.id) { idx, _ in
                    connectorCard(index: idx)
                }
                Button {
                    draft.addConnector()
                } label: {
                    Image(systemName: "plus")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(EjaTheme.inkMuted)
                        .frame(width: 26, height: 26)
                        .background(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .fill(EjaTheme.surface)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .stroke(EjaTheme.hairline, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 16)
        }
        .scrollIndicators(.never)
    }

    private func connectorCard(index: Int) -> some View {
        let conn = draft.connectors[index]
        let expanded = draft.expandedConnector == conn.id
        let status = client.connectorStatuses.first { $0.id == conn.id }
        let isHTTP = conn.transport.type == "http"
        let hasURL = !(conn.transport.url ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let dotColor: Color = {
            if !conn.enabled { return EjaTheme.inkMuted.opacity(0.35) }
            if status?.connected == true { return EjaTheme.positive }
            if let err = status?.error, !err.isEmpty { return EjaTheme.warning }
            return EjaTheme.inkMuted
        }()
        let enabledBinding = Binding<Bool>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].enabled : false },
            set: {
                if draft.connectors.indices.contains(index) {
                    draft.connectors[index].enabled = $0
                    draft.flushSave()
                }
            }
        )

        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Button {
                    withAnimation(.easeOut(duration: 0.14)) {
                        draft.expandedConnector = expanded ? nil : conn.id
                    }
                } label: {
                    HStack(spacing: 7) {
                        Circle()
                            .fill(dotColor)
                            .frame(width: 6, height: 6)
                        Text(conn.name.isEmpty ? conn.id : conn.name)
                            .font(.system(size: 12.5, weight: .medium))
                            .foregroundStyle(conn.enabled ? EjaTheme.ink : EjaTheme.inkSecondary)
                            .lineLimit(1)
                        if conn.enabled, status?.authRequired != true, let err = status?.error, !err.isEmpty {
                            Text(err)
                                .font(.system(size: 11))
                                .foregroundStyle(EjaTheme.warning)
                                .lineLimit(1)
                        }
                        Image(systemName: "chevron.right")
                            .font(.system(size: 8.5, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .rotationEffect(.degrees(expanded ? 90 : 0))
                        Spacer(minLength: 4)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                if let code = status?.oauthCode, !code.isEmpty {
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(code, forType: .string)
                    } label: {
                        Text(code)
                            .font(.system(size: 10.5, weight: .medium, design: .monospaced))
                            .foregroundStyle(EjaTheme.ink)
                            .padding(.horizontal, 6.5)
                            .padding(.vertical, 2.5)
                            .background(
                                RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                    .fill(EjaTheme.canvas)
                            )
                            .overlay(
                                RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                    .stroke(EjaTheme.hairlineStrong, lineWidth: 1)
                            )
                    }
                    .buttonStyle(.plain)
                } else if isHTTP && hasURL && status?.authRequired == true {
                    Button {
                        draft.flushSave()
                        client.oauthConnector(id: conn.id)
                    } label: {
                        Text("授权")
                            .font(.system(size: 10.5, weight: .medium))
                            .foregroundStyle(EjaTheme.canvas)
                            .padding(.horizontal, 7.5)
                            .padding(.vertical, 2.5)
                            .background(
                                RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                    .fill(EjaTheme.ink)
                            )
                    }
                    .buttonStyle(.plain)
                }

                if expanded {
                    if isHTTP && hasURL && status?.authRequired != true && (status?.oauthCode ?? "").isEmpty {
                        Button {
                            draft.flushSave()
                            client.oauthConnector(id: conn.id)
                        } label: {
                            Image(systemName: "key")
                                .font(.system(size: 10.5, weight: .medium))
                        }
                        .buttonStyle(EjaIconButtonStyle(size: 22))
                    }

                    Button {
                        draft.flushSave()
                        client.reloadConnectors()
                    } label: {
                        Image(systemName: "arrow.clockwise")
                            .font(.system(size: 10.5, weight: .medium))
                    }
                    .buttonStyle(EjaIconButtonStyle(size: 22))

                    Button {
                        draft.removeConnector(id: conn.id)
                    } label: {
                        Image(systemName: "trash")
                            .font(.system(size: 10.5))
                    }
                    .buttonStyle(EjaIconButtonStyle(size: 22, destructive: true))
                }

                Toggle("", isOn: enabledBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.mini)
            }
            .padding(.horizontal, 11)
            .padding(.vertical, 9)

            if let status, status.connected, !status.tools.isEmpty {
                FlowLayout(spacing: 5, lineSpacing: 5) {
                    ForEach(status.tools) { tool in
                        Text(tool.name)
                            .font(.system(size: 10.5, design: .monospaced))
                            .foregroundStyle(EjaTheme.inkSecondary)
                            .padding(.horizontal, 6.5)
                            .padding(.vertical, 2.5)
                            .background(
                                RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                    .fill(EjaTheme.canvas)
                            )
                            .overlay(
                                RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                    .stroke(EjaTheme.hairline, lineWidth: 1)
                            )
                    }
                }
                .padding(.horizontal, 11)
                .padding(.bottom, 9)
            }

            if expanded {
                connectorEditor(index: index)
            }
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

    private func connectorEditor(index: Int) -> some View {
        let conn = draft.connectors[index]
        let nameBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].name : "" },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].name = $0 } }
        )
        let urlBinding = transportBinding(index, \.url, "")
        let cmdLineBinding = commandLineBinding(index)
        let kvBinding = transportKVBinding(index)
        let isHTTP = conn.transport.type == "http"

        return VStack(alignment: .leading, spacing: 6) {
            divider
            HStack(spacing: 6) {
                TextField("", text: nameBinding, prompt: Text("名称").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 12))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())

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
            }

            if isHTTP {
                TextField("", text: urlBinding, prompt: Text("URL").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            } else {
                TextField("", text: cmdLineBinding, prompt: Text("命令").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }

            TextField("", text: kvBinding, prompt: Text("KEY=VALUE").foregroundStyle(EjaTheme.inkMuted), axis: .vertical)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(EjaTheme.ink)
                .textFieldStyle(.plain)
                .lineLimit(1...4)
                .modifier(CardField())
        }
        .padding(.horizontal, 11)
        .padding(.bottom, 10)
    }

    private func transportSegment(_ label: String, tag: String, index: Int) -> some View {
        let active = draft.connectors.indices.contains(index) && draft.connectors[index].transport.type == tag
        return Button {
            guard draft.connectors.indices.contains(index),
                  draft.connectors[index].transport.type != tag else { return }
            withAnimation(.easeOut(duration: 0.14)) {
                draft.connectors[index].transport = tag == "http"
                    ? ConnectorTransportDTO.http(url: draft.connectors[index].transport.url ?? "")
                    : ConnectorTransportDTO.stdio(
                        command: draft.connectors[index].transport.command ?? "",
                        args: draft.connectors[index].transport.args ?? []
                    )
                let conn = draft.connectors[index]
                draft.rawCommandLines[conn.id] = SettingsDraft.formatCommandLine(conn.transport)
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

    // MARK: - 通用辅助

    private var divider: some View {
        Rectangle()
            .fill(EjaTheme.hairline)
            .frame(height: 1)
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

    private func commandLineBinding(_ index: Int) -> Binding<String> {
        Binding(
            get: {
                guard draft.connectors.indices.contains(index) else { return "" }
                let conn = draft.connectors[index]
                return draft.rawCommandLines[conn.id] ?? SettingsDraft.formatCommandLine(conn.transport)
            },
            set: { raw in
                guard draft.connectors.indices.contains(index) else { return }
                let id = draft.connectors[index].id
                draft.rawCommandLines[id] = raw
                let parsed = SettingsDraft.parseCommandLine(raw)
                draft.connectors[index].transport.command = parsed.command
                draft.connectors[index].transport.args = parsed.args
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

struct FlowLayout: Layout {
    var spacing: CGFloat = 6
    var lineSpacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let maxWidth = proposal.width ?? .infinity
        var x: CGFloat = 0
        var y: CGFloat = 0
        var rowHeight: CGFloat = 0
        var usedWidth: CGFloat = 0

        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if x > 0 && x + size.width > maxWidth {
                x = 0
                y += rowHeight + lineSpacing
                rowHeight = 0
            }
            usedWidth = max(usedWidth, min(maxWidth, x + size.width))
            rowHeight = max(rowHeight, size.height)
            x += size.width + spacing
        }
        return CGSize(width: proposal.width ?? usedWidth, height: y + rowHeight)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = bounds.minX
        var y = bounds.minY
        var rowHeight: CGFloat = 0

        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if x > bounds.minX && x + size.width > bounds.maxX {
                x = bounds.minX
                y += rowHeight + lineSpacing
                rowHeight = 0
            }
            subview.place(
                at: CGPoint(x: x, y: y),
                anchor: .topLeading,
                proposal: ProposedViewSize(width: min(size.width, bounds.width), height: size.height)
            )
            rowHeight = max(rowHeight, size.height)
            x += size.width + spacing
        }
    }
}

private struct CardField: ViewModifier {
    func body(content: Content) -> some View {
        content
            .padding(.horizontal, 8)
            .padding(.vertical, 5.5)
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
