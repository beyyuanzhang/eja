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
    @Published var fetchingProviders: Set<String> = []
    @Published var fetchStatus: [String: String] = [:]
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
            autoSyncLocalProviders()
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
              let idx = providers.firstIndex(where: { $0.id == providerId }) else { return }
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
        let newProvider = ProviderConfigDTO(
            id: uniqueId,
            name: preset.name,
            kind: preset.kind,
            baseUrl: preset.baseUrl,
            apiKey: "",
            models: preset.models
        )
        providers.append(newProvider)
        if preset.baseUrl.isEmpty || (!Self.isLocalEndpoint(preset.baseUrl) && preset.models.isEmpty) {
            expandedProvider = uniqueId
        } else {
            expandedProvider = nil
        }
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
        let transport = ConnectorTransportDTO.stdio(command: "npx", args: ["-y", "@modelcontextprotocol/server-filesystem", NSHomeDirectory()])
        rawCommandLines[uniqueId] = Self.formatCommandLine(transport)
        connectors.append(ConnectorConfigDTO(id: uniqueId, name: "新连接器", enabled: true, transport: transport))
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
            try? await Task.sleep(nanoseconds: 380_000_000)
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

    func autoSyncLocalProviders() {
        for provider in providers where Self.isLocalEndpoint(provider.baseUrl) {
            fetchModels(for: provider.id, silent: true)
        }
    }

    func fetchModels(for providerId: String, selectFirstIfNeeded: Bool = false, silent: Bool = false) {
        guard let index = providers.firstIndex(where: { $0.id == providerId }) else { return }
        let provider = providers[index]
        let rawBase = provider.baseUrl.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !rawBase.isEmpty else { return }

        let cleanBase = rawBase.hasSuffix("/") ? String(rawBase.dropLast()) : rawBase
        let modelsURLString: String
        if provider.kind == "anthropic" && !cleanBase.hasSuffix("/v1") {
            modelsURLString = "\(cleanBase)/v1/models"
        } else {
            modelsURLString = "\(cleanBase)/models"
        }
        guard let url = URL(string: modelsURLString) else {
            if !silent { fetchStatus[providerId] = "URL 无效" }
            return
        }

        let apiKey = resolveApiKey(for: provider)
        fetchingProviders.insert(providerId)
        if !silent { fetchStatus[providerId] = "同步中…" }

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

            do {
                let (data, response) = try await URLSession.shared.data(for: req)
                guard let http = response as? HTTPURLResponse else {
                    if !silent { self?.fetchStatus[providerId] = "无响应" }
                    return
                }
                guard (200..<300).contains(http.statusCode) else {
                    if !silent { self?.fetchStatus[providerId] = "HTTP \(http.statusCode)" }
                    return
                }
                let discovered = Self.parseModelIds(from: data)
                guard !discovered.isEmpty else {
                    if !silent { self?.fetchStatus[providerId] = "未发现模型" }
                    return
                }
                guard let self,
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
                self.fetchStatus[providerId] = "\(merged.count) 个模型"
                self.flushSave()
            } catch {
                if !silent {
                    self?.fetchStatus[providerId] = "连接失败"
                }
            }
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

    static func hostSummary(_ baseUrl: String) -> String {
        let trimmed = baseUrl.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return "未配置地址" }
        if let url = URL(string: trimmed), let host = url.host {
            if let port = url.port {
                return "\(host):\(port)"
            }
            return host
        }
        return trimmed
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
        .init(id: "magpie", name: "Magpie 本地网关", kind: "openai", baseUrl: detectMagpieBaseUrl(), models: []),
        .init(id: "openai", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", models: ["gpt-4o", "gpt-4o-mini"]),
        .init(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", models: ["claude-sonnet-4-5", "claude-haiku-4-5"]),
        .init(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", models: ["deepseek-chat", "deepseek-reasoner"]),
        .init(id: "kimi", name: "Kimi Code", kind: "openai", baseUrl: "https://api.kimi.com/coding/v1", models: ["k2.5", "k2-thinking"]),
        .init(id: "qwen", name: "Qwen", kind: "openai", baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1", models: ["qwen-max", "qwen-plus"]),
        .init(id: "openrouter", name: "OpenRouter", kind: "openai", baseUrl: "https://openrouter.ai/api/v1", models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]),
        .init(id: "ollama", name: "Ollama 本地", kind: "openai", baseUrl: "http://127.0.0.1:11434/v1", models: []),
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
        .frame(width: 660, height: 470)
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
        VStack(alignment: .leading, spacing: 4) {
            sideItem(.models, title: "模型", icon: "cpu")
            sideItem(.connectors, title: "连接器", icon: "cable.connector")
            Spacer(minLength: 0)

            HStack(spacing: 5) {
                Image(systemName: "checkmark.circle.fill")
                    .font(.system(size: 10))
                Text("已保存")
                    .font(.system(size: 11))
            }
            .foregroundStyle(EjaTheme.positive)
            .opacity(draft.savedFlash ? 1 : 0)
            .padding(.horizontal, 9)
            .animation(.easeOut(duration: 0.18), value: draft.savedFlash)

            Button {
                draft.flushSave()
                dismiss()
            } label: {
                Text("完成")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(EjaTheme.inkSecondary)
                    .frame(maxWidth: .infinity)
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
            .buttonStyle(.plain)
            .keyboardShortcut(.cancelAction)
            .padding(.bottom, 12)
        }
        .padding(.horizontal, 10)
        .padding(.top, 14)
        .frame(width: 144)
    }

    private func sideItem(_ section: SettingsSection, title: String, icon: String) -> some View {
        let active = draft.section == section
        return Button {
            withAnimation(.easeOut(duration: 0.14)) { draft.section = section }
        } label: {
            HStack(spacing: 7) {
                Image(systemName: icon)
                    .font(.system(size: 11.5, weight: active ? .medium : .regular))
                    .frame(width: 15)
                Text(title)
                    .font(.system(size: 12.5, weight: active ? .medium : .regular))
            }
            .foregroundStyle(active ? EjaTheme.ink : EjaTheme.inkSecondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 9)
            .padding(.vertical, 6.5)
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

    // MARK: - 模型 (Magpie Core Logic)

    private var modelsPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                modelsHeaderBar
                if draft.providers.isEmpty {
                    emptyProvidersCard
                } else {
                    ForEach(Array(draft.providers.enumerated()), id: \.element.id) { idx, _ in
                        providerCard(index: idx)
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 14)
            .padding(.bottom, 20)
        }
        .scrollIndicators(.never)
    }

    private var modelsHeaderBar: some View {
        let activeProvider = draft.providers.first { $0.id == draft.judge.providerId }
        let hasSelection = activeProvider != nil && !draft.judge.model.trimmingCharacters(in: .whitespaces).isEmpty
        return HStack(spacing: 8) {
            HStack(spacing: 6) {
                Circle()
                    .fill(hasSelection ? EjaTheme.positive : EjaTheme.inkMuted.opacity(0.45))
                    .frame(width: 6.5, height: 6.5)
                if let activeProvider, hasSelection {
                    Text(draft.judge.model)
                        .font(.system(size: 12, weight: .medium, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .lineLimit(1)
                    Text("· \(activeProvider.name.isEmpty ? activeProvider.id : activeProvider.name)")
                        .font(.system(size: 11.5))
                        .foregroundStyle(EjaTheme.inkSecondary)
                        .lineLimit(1)
                } else {
                    Text("点击下方任意模型标签即可切换主模型")
                        .font(.system(size: 11.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                }
            }

            Spacer(minLength: 8)

            addProviderMenu
        }
        .padding(.bottom, 2)
    }

    private var addProviderMenu: some View {
        Menu {
            Section("本地网关") {
                Button("Magpie 本地网关") {
                    draft.addProvider(ProviderPreset.all[0])
                }
                Button("Ollama 本地") {
                    if let ollama = ProviderPreset.all.first(where: { $0.id == "ollama" }) {
                        draft.addProvider(ollama)
                    }
                }
            }
            Section("常用云端") {
                ForEach(ProviderPreset.all.filter { !["magpie", "ollama", "custom"].contains($0.id) }) { preset in
                    Button(preset.name) { draft.addProvider(preset) }
                }
            }
            Section {
                if let custom = ProviderPreset.all.first(where: { $0.id == "custom" }) {
                    Button(custom.name) { draft.addProvider(custom) }
                }
            }
        } label: {
            HStack(spacing: 4) {
                Image(systemName: "plus")
                    .font(.system(size: 10, weight: .semibold))
                Text("添加供应商")
                    .font(.system(size: 11.5, weight: .medium))
            }
            .foregroundStyle(EjaTheme.ink)
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
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

    private var emptyProvidersCard: some View {
        VStack(spacing: 8) {
            Text("尚未配置模型供应商")
                .font(.system(size: 12.5, weight: .medium))
                .foregroundStyle(EjaTheme.ink)
            Text("如果本机运行了 Magpie 网关，点击右上角「添加供应商 → Magpie 本地网关」即可自动拉取全部模型。")
                .font(.system(size: 11.5))
                .foregroundStyle(EjaTheme.inkMuted)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 32)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(EjaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(EjaTheme.hairline, lineWidth: 1)
        )
    }

    private func providerCard(index: Int) -> some View {
        let provider = draft.providers[index]
        let expanded = draft.expandedProvider == provider.id
        let isFetching = draft.fetchingProviders.contains(provider.id)
        let testResult = client.providerTestResults[provider.id]
        let syncText = draft.fetchStatus[provider.id]
        let needsInlineKey = !expanded &&
            !SettingsDraft.isLocalEndpoint(provider.baseUrl) &&
            !provider.hasApiKey &&
            provider.apiKey.trimmingCharacters(in: .whitespaces).isEmpty

        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 7) {
                Text(provider.name.isEmpty ? provider.id : provider.name)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(EjaTheme.ink)
                    .lineLimit(1)

                Text(SettingsDraft.hostSummary(provider.baseUrl))
                    .font(.system(size: 10.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.inkMuted)
                    .lineLimit(1)

                if provider.kind == "anthropic" {
                    Text("Anthropic")
                        .font(.system(size: 9.5, weight: .medium))
                        .foregroundStyle(EjaTheme.inkSecondary)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1.5)
                        .background(
                            RoundedRectangle(cornerRadius: 4, style: .continuous)
                                .fill(EjaTheme.surfaceSubtle)
                        )
                }

                Spacer(minLength: 4)

                if let testResult {
                    Text(testResult.message)
                        .font(.system(size: 10.5))
                        .foregroundStyle(testResult.ok ? EjaTheme.positive : EjaTheme.danger)
                        .lineLimit(1)
                        .frame(maxWidth: 130, alignment: .trailing)
                } else if let syncText {
                    Text(syncText)
                        .font(.system(size: 10.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                        .lineLimit(1)
                }

                Button {
                    draft.fetchModels(for: provider.id)
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 11, weight: .medium))
                        .rotationEffect(.degrees(isFetching ? 180 : 0))
                        .animation(.easeInOut(duration: 0.25), value: isFetching)
                }
                .buttonStyle(EjaIconButtonStyle(size: 24))
                .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty || isFetching)
                .help("从接口同步模型列表 (/models)")

                Button("测试") {
                    let activeOnThis = draft.judge.providerId == provider.id ? draft.judge.model : ""
                    let testModel = !activeOnThis.isEmpty ? activeOnThis : (provider.models.first ?? "gpt-4o")
                    client.testProvider(provider, model: testModel)
                }
                .font(.system(size: 11))
                .foregroundStyle(EjaTheme.inkSecondary)
                .padding(.horizontal, 7)
                .padding(.vertical, 3.5)
                .background(
                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                        .fill(EjaTheme.canvas)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                        .stroke(EjaTheme.hairline, lineWidth: 1)
                )
                .buttonStyle(.plain)
                .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)

                Button {
                    withAnimation(.easeOut(duration: 0.15)) {
                        draft.expandedProvider = expanded ? nil : provider.id
                    }
                } label: {
                    Image(systemName: expanded ? "chevron.up" : "slider.horizontal.3")
                        .font(.system(size: 11, weight: .medium))
                }
                .buttonStyle(EjaIconButtonStyle(size: 24))
                .help("编辑接口与密钥")
            }
            .padding(.horizontal, 12)
            .padding(.top, 10)
            .padding(.bottom, 8)

            if needsInlineKey {
                HStack(spacing: 6) {
                    Image(systemName: "key")
                        .font(.system(size: 10.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                    SecureField("", text: providerBinding(index, \.apiKey, ""), prompt: Text("输入 API Key…").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .onSubmit {
                            draft.flushSave()
                            draft.fetchModels(for: provider.id)
                        }
                }
                .modifier(CardField())
                .padding(.horizontal, 12)
                .padding(.bottom, 8)
            }

            providerModelPills(index: index)
                .padding(.horizontal, 12)
                .padding(.bottom, 11)

            if expanded {
                providerEditor(index: index)
            }
        }
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(EjaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(draft.judge.providerId == provider.id ? EjaTheme.hairlineStrong : EjaTheme.hairline, lineWidth: 1)
        )
    }

    private func providerModelPills(index: Int) -> some View {
        let provider = draft.providers[index]
        let isAdding = draft.addingModelProvider == provider.id
        return FlowLayout(spacing: 6, lineSpacing: 6) {
            ForEach(provider.models, id: \.self) { model in
                let selected = draft.judge.providerId == provider.id && draft.judge.model == model
                Button {
                    draft.selectModel(providerId: provider.id, model: model)
                } label: {
                    HStack(spacing: 4.5) {
                        if selected {
                            Image(systemName: "checkmark")
                                .font(.system(size: 9, weight: .bold))
                        }
                        Text(model)
                            .font(.system(size: 11.5, weight: selected ? .medium : .regular, design: .monospaced))
                            .lineLimit(1)
                    }
                    .foregroundStyle(selected ? EjaTheme.canvas : EjaTheme.ink)
                    .padding(.horizontal, 8.5)
                    .padding(.vertical, 4.5)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(selected ? EjaTheme.ink : EjaTheme.canvas)
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .stroke(selected ? .clear : EjaTheme.hairline, lineWidth: 1)
                    )
                }
                .buttonStyle(.plain)
                .contextMenu {
                    Button("设为当前主模型") {
                        draft.selectModel(providerId: provider.id, model: model)
                    }
                    Button("测试此模型") {
                        client.testProvider(provider, model: model)
                    }
                    Button("复制模型 ID") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(model, forType: .string)
                    }
                    Divider()
                    Button("移除模型", role: .destructive) {
                        draft.removeModel(providerId: provider.id, model: model)
                    }
                }
            }

            if isAdding {
                HStack(spacing: 4) {
                    TextField("", text: $draft.newModelInput, prompt: Text("输入模型 ID 回车").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .frame(width: 155)
                        .onSubmit {
                            draft.addModel(providerId: provider.id, model: draft.newModelInput)
                        }
                    Button {
                        draft.addModel(providerId: provider.id, model: draft.newModelInput)
                    } label: {
                        Image(systemName: "return")
                            .font(.system(size: 9.5, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkSecondary)
                    }
                    .buttonStyle(.plain)
                    Button {
                        draft.addingModelProvider = nil
                        draft.newModelInput = ""
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 9.5, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkMuted)
                    }
                    .buttonStyle(.plain)
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(EjaTheme.canvas)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .stroke(EjaTheme.hairlineStrong, lineWidth: 1)
                )
            } else {
                Button {
                    draft.newModelInput = ""
                    draft.addingModelProvider = provider.id
                } label: {
                    HStack(spacing: 3) {
                        Image(systemName: "plus")
                            .font(.system(size: 9.5, weight: .semibold))
                        Text("模型")
                            .font(.system(size: 11))
                    }
                    .foregroundStyle(EjaTheme.inkMuted)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4.5)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(EjaTheme.canvas.opacity(0.6))
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
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
        let keyPrompt = provider.hasApiKey
            ? "已保存密钥，留空沿用"
            : (SettingsDraft.isLocalEndpoint(provider.baseUrl) ? "本地网关通常无需密钥" : "sk-…")

        return VStack(alignment: .leading, spacing: 8) {
            divider
            HStack(spacing: 8) {
                labeled("名称") {
                    TextField("", text: nameBinding, prompt: Text("供应商名称").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }

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

            labeled("地址") {
                TextField("", text: urlBinding, prompt: Text("http://127.0.0.1:3425/v1").foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }

            labeled("密钥") {
                SecureField("", text: keyBinding, prompt: Text(keyPrompt).foregroundStyle(EjaTheme.inkMuted))
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .modifier(CardField())
            }

            HStack(spacing: 6) {
                Text("右键任意模型标签可测试或移除")
                    .font(.system(size: 10.5))
                    .foregroundStyle(EjaTheme.inkMuted)

                Spacer(minLength: 0)

                Button("同步模型") {
                    draft.fetchModels(for: provider.id)
                }
                .buttonStyle(EjaSecondaryButtonStyle(bordered: true))
                .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)

                Button("删除供应商") {
                    draft.removeProvider(id: provider.id)
                }
                .buttonStyle(EjaSecondaryButtonStyle(destructive: true))
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

    // MARK: - 连接器 (ChatGPT Connectors Logic)

    private var connectorsPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                connectorsHeaderBar
                if draft.connectors.isEmpty {
                    emptyConnectorsCard
                } else {
                    ForEach(Array(draft.connectors.enumerated()), id: \.element.id) { idx, _ in
                        connectorCard(index: idx)
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 14)
            .padding(.bottom, 20)
        }
        .scrollIndicators(.never)
    }

    private var connectorsHeaderBar: some View {
        let enabledCount = draft.connectors.filter(\.enabled).count
        let connectedCount = client.connectorStatuses.filter(\.connected).count
        let toolCount = client.connectedToolCount
        return HStack(spacing: 8) {
            HStack(spacing: 6) {
                Circle()
                    .fill(connectedCount > 0 ? EjaTheme.positive : EjaTheme.inkMuted.opacity(0.45))
                    .frame(width: 6.5, height: 6.5)
                if draft.connectors.isEmpty {
                    Text("连接 MCP 工具扩展本地与外部能力")
                        .font(.system(size: 11.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                } else {
                    Text("已启用 \(enabledCount) 个连接器 · \(toolCount) 个工具可用")
                        .font(.system(size: 11.5))
                        .foregroundStyle(EjaTheme.inkSecondary)
                }
            }

            Spacer(minLength: 8)

            if !draft.connectors.isEmpty {
                Button {
                    draft.flushSave()
                    client.reloadConnectors()
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "arrow.clockwise")
                            .font(.system(size: 10, weight: .medium))
                        Text("重连")
                            .font(.system(size: 11.5))
                    }
                    .foregroundStyle(EjaTheme.inkSecondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
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

            Button {
                draft.addConnector()
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: "plus")
                        .font(.system(size: 10, weight: .semibold))
                    Text("添加连接器")
                        .font(.system(size: 11.5, weight: .medium))
                }
                .foregroundStyle(EjaTheme.ink)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
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
        .padding(.bottom, 2)
    }

    private var emptyConnectorsCard: some View {
        VStack(spacing: 8) {
            Text("尚未添加任何连接器")
                .font(.system(size: 12.5, weight: .medium))
                .foregroundStyle(EjaTheme.ink)
            Text("支持标准 MCP 本地命令（如 npx / uvx / python）与远程 HTTP 服务，连接后工具将自动开放给内核。")
                .font(.system(size: 11.5))
                .foregroundStyle(EjaTheme.inkMuted)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 32)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(EjaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(EjaTheme.hairline, lineWidth: 1)
        )
    }

    private func connectorCard(index: Int) -> some View {
        let conn = draft.connectors[index]
        let expanded = draft.expandedConnector == conn.id
        let status = client.connectorStatuses.first { $0.id == conn.id }
        let dotColor: Color = {
            if !conn.enabled { return EjaTheme.inkMuted.opacity(0.35) }
            if status?.connected == true { return EjaTheme.positive }
            if let err = status?.error, !err.isEmpty { return EjaTheme.warning }
            return EjaTheme.inkMuted
        }()
        let subtitleInfo = connectorSubtitle(conn: conn, status: status)
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
            HStack(spacing: 10) {
                Button {
                    withAnimation(.easeOut(duration: 0.16)) {
                        draft.expandedConnector = expanded ? nil : conn.id
                    }
                } label: {
                    HStack(spacing: 9) {
                        Circle()
                            .fill(dotColor)
                            .frame(width: 7, height: 7)

                        VStack(alignment: .leading, spacing: 2) {
                            Text(conn.name.isEmpty ? conn.id : conn.name)
                                .font(.system(size: 13, weight: .medium))
                                .foregroundStyle(conn.enabled ? EjaTheme.ink : EjaTheme.inkSecondary)
                                .lineLimit(1)
                            Text(subtitleInfo.text)
                                .font(.system(size: 11))
                                .foregroundStyle(subtitleInfo.color)
                                .lineLimit(1)
                        }

                        Spacer(minLength: 4)

                        Image(systemName: "chevron.right")
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .rotationEffect(.degrees(expanded ? 90 : 0))
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                Toggle("", isOn: enabledBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)

            if let status, status.connected, !status.tools.isEmpty {
                let visibleTools = expanded ? status.tools : Array(status.tools.prefix(6))
                FlowLayout(spacing: 5, lineSpacing: 5) {
                    ForEach(visibleTools) { tool in
                        Text(tool.name)
                            .font(.system(size: 10.5, design: .monospaced))
                            .foregroundStyle(EjaTheme.inkSecondary)
                            .padding(.horizontal, 7)
                            .padding(.vertical, 3)
                            .background(
                                RoundedRectangle(cornerRadius: 5, style: .continuous)
                                    .fill(EjaTheme.canvas)
                            )
                            .overlay(
                                RoundedRectangle(cornerRadius: 5, style: .continuous)
                                    .stroke(EjaTheme.hairline, lineWidth: 1)
                            )
                            .help(tool.description)
                    }
                    if !expanded && status.tools.count > 6 {
                        Text("+\(status.tools.count - 6)")
                            .font(.system(size: 10.5, design: .monospaced))
                            .foregroundStyle(EjaTheme.inkMuted)
                            .padding(.horizontal, 6)
                            .padding(.vertical, 3)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.bottom, 10)
            }

            if expanded {
                connectorEditor(index: index)
            }
        }
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(EjaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(EjaTheme.hairline, lineWidth: 1)
        )
    }

    private func connectorSubtitle(conn: ConnectorConfigDTO, status: ConnectorStatusDTO?) -> (text: String, color: Color) {
        let summary = transportSummary(conn.transport)
        if !conn.enabled {
            return ("已停用 · \(summary)", EjaTheme.inkMuted)
        }
        if let status {
            if status.connected {
                return ("已连接 · \(status.tools.count) 个工具 · \(summary)", EjaTheme.positive)
            }
            if let err = status.error, !err.isEmpty {
                return (err, EjaTheme.warning)
            }
        }
        return ("正在连接 · \(summary)", EjaTheme.inkMuted)
    }

    private func transportSummary(_ transport: ConnectorTransportDTO) -> String {
        if transport.type == "http" {
            let url = (transport.url ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return url.isEmpty ? "远程 HTTP" : url
        }
        let cmd = SettingsDraft.formatCommandLine(transport)
        return cmd.isEmpty ? "本地命令" : cmd
    }

    private func connectorEditor(index: Int) -> some View {
        let conn = draft.connectors[index]
        let status = client.connectorStatuses.first { $0.id == conn.id }
        let nameBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].name : "" },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].name = $0 } }
        )
        let urlBinding = transportBinding(index, \.url, "")
        let cmdLineBinding = commandLineBinding(index)
        let kvBinding = transportKVBinding(index)
        let isHTTP = conn.transport.type == "http"

        return VStack(alignment: .leading, spacing: 8) {
            divider

            if let err = status?.error, !err.isEmpty, conn.enabled {
                HStack(alignment: .top, spacing: 6) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 11))
                        .foregroundStyle(EjaTheme.warning)
                    Text(err)
                        .font(.system(size: 11))
                        .foregroundStyle(EjaTheme.warning)
                        .textSelection(.enabled)
                    Spacer(minLength: 4)
                }
                .padding(8)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(EjaTheme.canvas)
                )
            }

            HStack(spacing: 8) {
                labeled("名称") {
                    TextField("", text: nameBinding, prompt: Text("例如：Filesystem / GitHub").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }

                HStack(spacing: 2) {
                    transportSegment("本地命令", tag: "stdio", index: index)
                    transportSegment("远程地址", tag: "http", index: index)
                }
                .padding(2)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(EjaTheme.canvas)
                )
                .fixedSize()
            }

            if isHTTP {
                labeled("地址") {
                    TextField("", text: urlBinding, prompt: Text("http://127.0.0.1:8080/mcp").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }
                labeled("头部") {
                    TextField("", text: kvBinding, prompt: Text("可选，KEY=VALUE 每行一项").foregroundStyle(EjaTheme.inkMuted), axis: .vertical)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .lineLimit(1...4)
                        .modifier(CardField())
                }
            } else {
                labeled("命令") {
                    TextField("", text: cmdLineBinding, prompt: Text("npx -y @modelcontextprotocol/server-filesystem /Users/bey").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .modifier(CardField())
                }
                labeled("环境") {
                    TextField("", text: kvBinding, prompt: Text("可选，KEY=VALUE 每行一项").foregroundStyle(EjaTheme.inkMuted), axis: .vertical)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .lineLimit(1...4)
                        .modifier(CardField())
                }
            }

            HStack(spacing: 6) {
                Text(isHTTP ? "连接远程 Streamable HTTP / SSE MCP 服务" : "直接粘贴完整命令与参数，空格自动解析")
                    .font(.system(size: 10.5))
                    .foregroundStyle(EjaTheme.inkMuted)

                Spacer(minLength: 0)

                Button("保存并重连") {
                    draft.flushSave()
                    client.reloadConnectors()
                }
                .buttonStyle(EjaSecondaryButtonStyle(bordered: true))

                Button("删除") {
                    draft.removeConnector(id: conn.id)
                }
                .buttonStyle(EjaSecondaryButtonStyle(destructive: true))
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
                        command: draft.connectors[index].transport.command ?? "npx",
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

/// 自适应换行流式布局（用于模型胶囊与工具标签）。
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
