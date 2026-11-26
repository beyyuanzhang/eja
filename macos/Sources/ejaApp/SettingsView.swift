import AppKit
import ejaClient
import SwiftUI

enum SettingsSection: Hashable {
    case models
    case capabilities
}

@MainActor
final class SettingsDraft: ObservableObject {
    @Published var section: SettingsSection = .models
    @Published var providers: [ProviderConfigDTO] = []
    @Published var judge = ModelRefDTO()
    @Published var capabilities: [CapabilityConfigDTO] = []
    @Published var expandedProvider: String?
    @Published var expandedCapability: String?
    @Published var fetchingProviders: Set<String> = []
    @Published var addingModelProvider: String?
    @Published var newModelInput: String = ""
    @Published var rawCommandLines: [String: String] = [:]
    @Published var rawTransportKVs: [String: String] = [:]

    private var saved = HostConfigDTO()
    private var pendingSave: (id: UUID, config: HostConfigDTO)?
    private weak var client: HostClient?
    private var saveTask: Task<Void, Never>?

    func attach(client: HostClient) {
        self.client = client
    }

    func load(from config: HostConfigDTO) {
        providers = config.providers
        judge = config.judge
        applyCapabilities(config.capabilities)
        saved = config

        if healJudgeModelIfNeeded() {
            scheduleSave()
        }
    }

    func syncWithHost(_ latest: HostConfigDTO) {
        guard pendingSave == nil else { return }
        applyConfirmed(latest, matching: saved)
    }

    private func applyConfirmed(_ latest: HostConfigDTO, matching baseline: HostConfigDTO) {
        if providers == baseline.providers { providers = latest.providers }
        if judge == baseline.judge { judge = latest.judge }
        if capabilities == baseline.capabilities { applyCapabilities(latest.capabilities) }
        saved = latest
    }

    private func applyCapabilities(_ latest: [CapabilityConfigDTO]) {
        let previous = capabilities
        capabilities = latest
        let ids = Set(latest.map(\.id))
        rawCommandLines = rawCommandLines.filter { ids.contains($0.key) }
        rawTransportKVs = rawTransportKVs.filter { ids.contains($0.key) }
        for cap in latest {
            let changed = previous.first(where: { $0.id == cap.id })?.transport != cap.transport
            if rawCommandLines[cap.id] == nil || changed {
                rawCommandLines[cap.id] = Self.formatCommandLine(cap.transport)
            }
            if rawTransportKVs[cap.id] == nil || changed {
                rawTransportKVs[cap.id] = Self.formatTransportKV(cap.transport)
            }
        }
        if let id = expandedCapability, !latest.contains(where: { $0.id == id }) {
            expandedCapability = nil
        }
    }

    private var current: HostConfigDTO {
        HostConfigDTO(providers: providers, judge: judge, capabilities: capabilities)
    }

    private var lastSubmitted: HostConfigDTO {
        pendingSave?.config ?? saved
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
            fetchModels(for: uniqueId)
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

    func changeEndpoint(_ index: Int, field: WritableKeyPath<ProviderConfigDTO, String>, to value: String) {
        guard providers.indices.contains(index), providers[index][keyPath: field] != value else { return }
        providers[index][keyPath: field] = value
        providers[index].apiKey = ""
        providers[index].hasApiKey = false
    }

    func addCapability() {
        var uniqueId = "cap-\(capabilities.count + 1)"
        var n = capabilities.count + 2
        while capabilities.contains(where: { $0.id == uniqueId }) {
            uniqueId = "cap-\(n)"
            n += 1
        }
        let transport = CapabilityTransportDTO.exec(command: "", args: [])
        rawCommandLines[uniqueId] = ""
        rawTransportKVs[uniqueId] = ""
        capabilities.append(CapabilityConfigDTO(id: uniqueId, name: "", enabled: true, transport: transport))
        expandedCapability = uniqueId
    }

    func removeCapability(id: String) {
        capabilities.removeAll { $0.id == id && $0.transport.type != "builtin" }
        rawCommandLines.removeValue(forKey: id)
        rawTransportKVs.removeValue(forKey: id)
        if expandedCapability == id { expandedCapability = nil }
        flushSave()
    }

    func scheduleSave() {
        guard providers != lastSubmitted.providers || judge != lastSubmitted.judge
            || capabilities != lastSubmitted.capabilities else { return }
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
        guard let client else { return }
        let draft = current
        let previous = lastSubmitted
        let modelsChanged = draft.providers != previous.providers || draft.judge != previous.judge
        let capabilitiesChanged = draft.capabilities != previous.capabilities
        guard modelsChanged || capabilitiesChanged else { return }
        let id = UUID()
        pendingSave = (id, draft)
        client.saveConfig(
            providers: modelsChanged ? draft.providers : nil,
            judge: modelsChanged ? draft.judge : nil,
            capabilities: capabilitiesChanged ? draft.capabilities : nil,
            id: id
        ) { [weak self] confirmed in
            guard let self, self.pendingSave?.id == id else { return }
            self.pendingSave = nil
            self.applyConfirmed(confirmed, matching: draft)
        }
    }

    func fetchModels(for providerId: String) {
        guard let client, let provider = providers.first(where: { $0.id == providerId }),
              !provider.baseUrl.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              fetchingProviders.insert(providerId).inserted else { return }
        client.listModels(for: provider) { [weak self] discovered in
            defer { self?.fetchingProviders.remove(providerId) }
            guard let discovered, !discovered.isEmpty,
                  let self,
                  let idx = self.providers.firstIndex(where: { $0.id == providerId }),
                  self.providers[idx].kind == provider.kind,
                  self.providers[idx].baseUrl == provider.baseUrl,
                  self.providers[idx].apiKey.isEmpty || self.providers[idx].apiKey == provider.apiKey else { return }

            let existing = self.providers[idx].models.filter { model in
                !discovered.contains(model) && !discovered.contains { $0.lowercased().hasSuffix("/" + model.lowercased()) }
            }
            self.providers[idx].models = discovered + existing
            _ = self.healJudgeModelIfNeeded()
            self.flushSave()
        }
    }

    static func formatCommandLine(_ transport: CapabilityTransportDTO) -> String {
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

    static func formatTransportKV(_ transport: CapabilityTransportDTO) -> String {
        let map = transport.type == "http" ? (transport.headers ?? [:]) : (transport.env ?? [:])
        return map.keys.sorted().map { "\($0)=\(map[$0] ?? "")" }.joined(separator: "\n")
    }

    static func parseTransportKV(_ raw: String) -> [String: String] {
        var map: [String: String] = [:]
        for line in raw.split(whereSeparator: \.isNewline) {
            if let sep = line.firstIndex(of: "=") {
                let key = String(line[..<sep]).trimmingCharacters(in: .whitespaces)
                let value = String(line[line.index(after: sep)...]).trimmingCharacters(in: .whitespaces)
                if !key.isEmpty { map[key] = value }
            }
        }
        return map
    }
}

struct ProviderPreset: Identifiable {
    let id: String
    let name: String
    let kind: String
    let baseUrl: String
    let models: [String]

    static let all: [ProviderPreset] = [
        .init(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", models: ["claude-sonnet-4-5", "claude-haiku-4-5"]),
        .init(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", models: ["deepseek-chat", "deepseek-reasoner"]),
        .init(id: "kimi", name: "Kimi", kind: "openai", baseUrl: "https://api.kimi.com/coding/v1", models: ["k2.5", "k2-thinking"]),
        .init(id: "ollama", name: "Ollama", kind: "openai", baseUrl: "http://127.0.0.1:11434/v1", models: []),
        .init(id: "openai", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", models: ["gpt-4o", "gpt-4o-mini"]),
        .init(id: "openrouter", name: "OpenRouter", kind: "openai", baseUrl: "https://openrouter.ai/api/v1", models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]),
        .init(id: "qwen", name: "Qwen", kind: "openai", baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1", models: ["qwen-max", "qwen-plus"]),
        .init(id: "custom", name: "", kind: "openai", baseUrl: "", models: []),
    ]
}

struct SettingsView: View {
    @ObservedObject var client: HostClient
    @StateObject private var draft = SettingsDraft()

    var body: some View {
        VStack(spacing: 0) {
            tabs
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(ejaTheme.canvas.ignoresSafeArea())
        .tint(ejaTheme.ink)
        .onAppear {
            draft.attach(client: client)
            draft.load(from: client.config)
        }
        .onChange(of: client.config) { _, latest in
            draft.syncWithHost(latest)
        }
        .onChange(of: draft.providers) { _, _ in draft.scheduleSave() }
        .onChange(of: draft.judge) { _, _ in draft.scheduleSave() }
        .onChange(of: draft.capabilities) { _, _ in draft.scheduleSave() }
        .onDisappear { draft.flushSave() }
    }

    private var tabs: some View {
        HStack(spacing: 4) {
            tab(.models, icon: "brain")
            tab(.capabilities, icon: "point.3.connected.trianglepath.dotted")
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 16)
        .padding(.top, 4)
    }

    private func tab(_ section: SettingsSection, icon: String) -> some View {
        let active = draft.section == section
        return Button {
            withAnimation(.smooth) { draft.section = section }
        } label: {
            Image(systemName: icon)
                .font(.body)
                .foregroundStyle(active ? ejaTheme.ink : ejaTheme.inkSecondary)
                .padding(6)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(active ? ejaTheme.surfaceSubtle : .clear)
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
        case .capabilities:
            capabilitiesPane
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
            .padding(.top, 8)
            .padding(.bottom, 16)
        }
        .scrollIndicators(.never)
    }

    private var addProviderMenu: some View {
        Menu {
            ForEach(ProviderPreset.all) { preset in
                Button(preset.name.isEmpty ? "…" : preset.name) { draft.addProvider(preset) }
            }
        } label: {
            Image(systemName: "plus")
                .font(.subheadline.weight(.medium))
                .foregroundStyle(ejaTheme.inkMuted)
                .padding(7)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(ejaTheme.surface)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .stroke(ejaTheme.hairline, lineWidth: 1)
                )
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
    }

    private func providerCard(index: Int) -> some View {
        let provider = draft.providers[index]
        let expanded = draft.expandedProvider == provider.id
        let isFetching = draft.fetchingProviders.contains(provider.id)
        let testResult = client.providerTestResults[provider.id]
        let activeModel = draft.judge.providerId == provider.id ? draft.judge.model : ""

        return VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                title(providerBinding(index, \.name, ""), placeholder: provider.id)
                Button {
                    withAnimation(.smooth) {
                        draft.expandedProvider = expanded ? nil : provider.id
                    }
                } label: {
                    HStack(spacing: 6) {
                        chevron(expanded)
                        Spacer(minLength: 4)
                        if !expanded && !activeModel.isEmpty {
                            Text(activeModel)
                                .font(.system(.subheadline, design: .monospaced))
                                .foregroundStyle(ejaTheme.inkSecondary)
                                .lineLimit(1)
                        }
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityValue(expanded ? "已展开" : "已收起")

                if expanded {
                    if let testResult {
                        Image(systemName: testResult.ok ? "checkmark.circle.fill" : "xmark.circle.fill")
                            .font(.callout)
                            .foregroundStyle(testResult.ok ? ejaTheme.positive : ejaTheme.danger)
                            .help(testResult.message)
                    }

                    Button {
                        let model = !activeModel.isEmpty ? activeModel : (provider.models.first ?? "gpt-4o")
                        client.testProvider(provider, model: model)
                    } label: {
                        Image(systemName: "bolt")
                            .font(.subheadline.weight(.medium))
                    }
                    .buttonStyle(ejaIconButtonStyle())
                    .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)

                    Button {
                        draft.fetchModels(for: provider.id)
                    } label: {
                        Image(systemName: "arrow.clockwise")
                            .font(.subheadline.weight(.medium))
                            .rotationEffect(.degrees(isFetching ? 180 : 0))
                            .animation(.easeInOut(duration: 0.25), value: isFetching)
                    }
                    .buttonStyle(ejaIconButtonStyle())
                    .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty || isFetching)

                    Button {
                        draft.removeProvider(id: provider.id)
                    } label: {
                        Image(systemName: "trash")
                            .font(.subheadline)
                    }
                    .buttonStyle(ejaIconButtonStyle(destructive: true))
                }
            }

            if expanded {
                providerModelPills(index: index)
                Divider()
                providerEditor(index: index)
            }
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(ejaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .stroke(ejaTheme.hairline, lineWidth: 1)
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
                        .font(.system(.callout, design: .monospaced, weight: selected ? .medium : .regular))
                        .lineLimit(1)
                        .foregroundStyle(selected ? ejaTheme.canvas : ejaTheme.ink)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .background(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .fill(selected ? ejaTheme.ink : ejaTheme.canvas)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .stroke(selected ? .clear : ejaTheme.hairline, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
                .contextMenu {
                    Button { client.testProvider(provider, model: model) } label: {
                        Image(systemName: "bolt")
                    }
                    Button(role: .destructive) {
                        draft.removeModel(providerId: provider.id, model: model)
                    } label: {
                        Image(systemName: "trash")
                    }
                }
            }

            if isAdding {
                TextField("", text: $draft.newModelInput, prompt: Text("模型").foregroundStyle(ejaTheme.inkMuted))
                    .font(.system(.subheadline, design: .monospaced))
                    .foregroundStyle(ejaTheme.ink)
                    .textFieldStyle(.plain)
                    .fixedSize(horizontal: true, vertical: false)
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
                            .fill(ejaTheme.canvas)
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                            .stroke(ejaTheme.hairlineStrong, lineWidth: 1)
                    )
            } else {
                Button {
                    draft.newModelInput = ""
                    draft.addingModelProvider = provider.id
                } label: {
                    Image(systemName: "plus")
                        .font(.caption.weight(.medium))
                        .foregroundStyle(ejaTheme.inkMuted)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 5.5)
                        .background(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .fill(ejaTheme.canvas)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 5.5, style: .continuous)
                                .stroke(ejaTheme.hairline, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
            }
        }
    }

    private func providerEditor(index: Int) -> some View {
        let provider = draft.providers[index]
        let urlBinding = Binding(
            get: { draft.providers.indices.contains(index) ? draft.providers[index].baseUrl : "" },
            set: { draft.changeEndpoint(index, field: \.baseUrl, to: $0) }
        )
        let keyBinding = providerBinding(index, \.apiKey, "")

        return VStack(spacing: 6) {
            HStack(spacing: 6) {
                field("URL", text: urlBinding)
                HStack(spacing: 2) {
                    kindSegment("OpenAI", tag: "openai", index: index)
                    kindSegment("Anthropic", tag: "anthropic", index: index)
                }
                .padding(2)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(ejaTheme.canvas)
                )
                .fixedSize()
            }

            SecureField("", text: keyBinding, prompt: Text(provider.hasApiKey ? "密钥 ••••" : "密钥").foregroundStyle(ejaTheme.inkMuted))
                .font(.system(.callout, design: .monospaced))
                .foregroundStyle(ejaTheme.ink)
                .textFieldStyle(.plain)
                .onSubmit {
                    draft.flushSave()
                    draft.fetchModels(for: provider.id)
                }
                .modifier(CardField())
        }
    }

    private func kindSegment(_ label: String, tag: String, index: Int) -> some View {
        let active = draft.providers.indices.contains(index) && draft.providers[index].kind == tag
        return Button {
            withAnimation(.easeOut(duration: 0.14)) {
                draft.changeEndpoint(index, field: \.kind, to: tag)
            }
        } label: {
            Text(label)
                .font(.subheadline.weight(active ? .medium : .regular))
                .foregroundStyle(active ? ejaTheme.ink : ejaTheme.inkSecondary)
                .padding(.horizontal, 8)
                .padding(.vertical, 3.5)
                .background {
                    if active {
                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                            .fill(ejaTheme.surface)
                            .shadow(color: ejaTheme.subtleShadow, radius: 2, x: 0, y: 1)
                    }
                }
        }
        .buttonStyle(.plain)
    }

    // MARK: - 能力

    private var capabilitiesPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(Array(draft.capabilities.enumerated()), id: \.element.id) { idx, _ in
                    capabilityCard(index: idx)
                }
                Button {
                    draft.addCapability()
                } label: {
                    Image(systemName: "plus")
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(ejaTheme.inkMuted)
                        .padding(7)
                        .background(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .fill(ejaTheme.surface)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .stroke(ejaTheme.hairline, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 16)
            .padding(.top, 8)
            .padding(.bottom, 16)
        }
        .scrollIndicators(.never)
    }

    private func capabilityCard(index: Int) -> some View {
        let cap = draft.capabilities[index]
        let isBuiltin = cap.transport.type == "builtin"
        let status = client.capabilityStatuses.first { $0.id == cap.id }
        let enabled = status?.enabled ?? false
        let savingSwitch = cap.enabled != enabled
        let tools = status?.connected == true ? (status?.tools ?? []) : []
        let error = enabled && status?.authRequired != true ? status?.error : nil
        let canExpand = !isBuiltin || !tools.isEmpty || error?.isEmpty == false
        let expanded = canExpand && draft.expandedCapability == cap.id
        let isHTTP = cap.transport.type == "http"
        let hasURL = !(cap.transport.url ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let dotColor: Color = {
            if !enabled && !isBuiltin { return ejaTheme.inkMuted.opacity(0.35) }
            if isBuiltin || status?.connected == true { return ejaTheme.positive }
            if let err = status?.error, !err.isEmpty { return ejaTheme.warning }
            return ejaTheme.inkMuted
        }()
        let nameBinding = Binding<String>(
            get: { draft.capabilities.indices.contains(index) ? draft.capabilities[index].name : "" },
            set: { if draft.capabilities.indices.contains(index) { draft.capabilities[index].name = $0 } }
        )
        let enabledBinding = Binding<Bool>(
            get: { enabled },
            set: {
                if draft.capabilities.indices.contains(index) {
                    draft.capabilities[index].enabled = $0
                    draft.flushSave()
                }
            }
        )

        return VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 6) {
                Image(systemName: "circle.fill")
                    .font(.caption2)
                    .imageScale(.small)
                    .foregroundStyle(dotColor)
                title(nameBinding, placeholder: cap.id)
                    .foregroundStyle((enabled || isBuiltin) ? ejaTheme.ink : ejaTheme.inkSecondary)
                    .allowsHitTesting(!isBuiltin)
                Button {
                    withAnimation(.smooth) {
                        draft.expandedCapability = expanded ? nil : cap.id
                    }
                } label: {
                    HStack(spacing: 6) {
                        if canExpand {
                            chevron(expanded)
                        }
                        Spacer(minLength: 4)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(!canExpand)
                .accessibilityValue(canExpand ? (expanded ? "已展开" : "已收起") : "")

                if let error, !error.isEmpty {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.subheadline)
                        .foregroundStyle(ejaTheme.warning)
                        .help(error)
                }

                if expanded && !isBuiltin {
                    if let code = status?.oauthCode, !code.isEmpty {
                            Button {
                                NSPasteboard.general.clearContents()
                                NSPasteboard.general.setString(code, forType: .string)
                            } label: {
                                Text(code)
                                    .font(.system(.caption, design: .monospaced, weight: .medium))
                                    .foregroundStyle(ejaTheme.ink)
                                    .padding(.horizontal, 6.5)
                                    .padding(.vertical, 2.5)
                                    .background(
                                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                            .fill(ejaTheme.canvas)
                                    )
                                    .overlay(
                                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                            .stroke(ejaTheme.hairlineStrong, lineWidth: 1)
                                    )
                            }
                            .buttonStyle(.plain)
                        } else if isHTTP && hasURL && status?.authRequired == true {
                            Button {
                                draft.flushSave()
                                client.oauthCapability(id: cap.id)
                            } label: {
                                Image(systemName: "key.fill")
                                    .font(.subheadline.weight(.medium))
                                    .foregroundStyle(ejaTheme.canvas)
                                    .padding(.horizontal, 7.5)
                                    .padding(.vertical, 2.5)
                                    .background(
                                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                            .fill(ejaTheme.ink)
                                    )
                            }
                            .buttonStyle(.plain)
                        }

                        Button {
                            draft.flushSave()
                            client.reloadCapabilities()
                        } label: {
                            Image(systemName: "arrow.clockwise")
                                .font(.subheadline.weight(.medium))
                        }
                        .buttonStyle(ejaIconButtonStyle())

                        Button {
                            draft.removeCapability(id: cap.id)
                        } label: {
                            Image(systemName: "trash")
                                .font(.subheadline)
                        }
                        .buttonStyle(ejaIconButtonStyle(destructive: true))
                }

                if !isBuiltin {
                    Toggle("启用能力", isOn: enabledBinding)
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .disabled(savingSwitch)
                }
            }

            if expanded {
                if !tools.isEmpty {
                    FlowLayout(spacing: 5, lineSpacing: 5) {
                        ForEach(tools) { tool in
                            Text(tool.name)
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(ejaTheme.inkSecondary)
                                .padding(.horizontal, 6.5)
                                .padding(.vertical, 2.5)
                                .background(
                                    RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                        .fill(ejaTheme.canvas)
                                )
                                .overlay(
                                    RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                                        .stroke(ejaTheme.hairline, lineWidth: 1)
                                )
                        }
                    }
                }

                if !isBuiltin {
                    Divider()
                    capabilityEditor(index: index)
                }
            }
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(ejaTheme.surface)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .stroke(ejaTheme.hairline, lineWidth: 1)
        )
    }

    private func capabilityEditor(index: Int) -> some View {
        let cap = draft.capabilities[index]
        let urlBinding = transportBinding(index, \.url, "")
        let descBinding = transportBinding(index, \.description, "")
        let cmdLineBinding = commandLineBinding(index)
        let kvBinding = transportKVBinding(index)
        let isHTTP = cap.transport.type == "http"
        let isExec = cap.transport.type == "exec"

        return VStack(spacing: 6) {
            HStack(spacing: 6) {
                if isHTTP {
                    field("URL", text: urlBinding)
                } else {
                    field("命令", text: cmdLineBinding)
                }
                HStack(spacing: 2) {
                    transportSegment("Exec", tag: "exec", index: index)
                    transportSegment("Stdio", tag: "stdio", index: index)
                    transportSegment("HTTP", tag: "http", index: index)
                }
                .padding(2)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(ejaTheme.canvas)
                )
                .fixedSize()
            }

            if isExec {
                field("描述", text: descBinding)
            }

            TextField("", text: kvBinding, prompt: Text("KEY=VALUE").foregroundStyle(ejaTheme.inkMuted), axis: .vertical)
                .font(.system(.subheadline, design: .monospaced))
                .foregroundStyle(ejaTheme.ink)
                .textFieldStyle(.plain)
                .lineLimit(1...4)
                .modifier(CardField())
        }
    }

    private func transportSegment(_ label: String, tag: String, index: Int) -> some View {
        let active = draft.capabilities.indices.contains(index) && draft.capabilities[index].transport.type == tag
        return Button {
            guard draft.capabilities.indices.contains(index),
                  draft.capabilities[index].transport.type != tag else { return }
            withAnimation(.easeOut(duration: 0.14)) {
                let prev = draft.capabilities[index].transport
                switch tag {
                case "http":
                    draft.capabilities[index].transport = CapabilityTransportDTO.http(
                        url: prev.url ?? "",
                        headers: prev.headers ?? [:]
                    )
                case "stdio":
                    draft.capabilities[index].transport = CapabilityTransportDTO.stdio(
                        command: prev.command ?? "",
                        args: prev.args ?? [],
                        env: prev.env ?? [:]
                    )
                default:
                    draft.capabilities[index].transport = CapabilityTransportDTO.exec(
                        command: prev.command ?? "",
                        args: prev.args ?? [],
                        env: prev.env ?? [:],
                        description: prev.description ?? "",
                        schema: prev.schema
                    )
                }
                let cap = draft.capabilities[index]
                draft.rawCommandLines[cap.id] = SettingsDraft.formatCommandLine(cap.transport)
                draft.rawTransportKVs[cap.id] = SettingsDraft.formatTransportKV(cap.transport)
            }
        } label: {
            Text(label)
                .font(.subheadline.weight(active ? .medium : .regular))
                .foregroundStyle(active ? ejaTheme.ink : ejaTheme.inkSecondary)
                .padding(.horizontal, 8)
                .padding(.vertical, 3.5)
                .background {
                    if active {
                        RoundedRectangle(cornerRadius: 4.5, style: .continuous)
                            .fill(ejaTheme.surface)
                            .shadow(color: ejaTheme.subtleShadow, radius: 2, x: 0, y: 1)
                    }
                }
        }
        .buttonStyle(.plain)
    }

    // MARK: - 通用辅助

    private func title(_ text: Binding<String>, placeholder: String) -> some View {
        TextField("", text: text, prompt: Text(placeholder).foregroundStyle(ejaTheme.inkMuted))
            .font(.body.weight(.medium))
            .textFieldStyle(.plain)
            .fixedSize()
    }

    private func chevron(_ expanded: Bool) -> some View {
        Image(systemName: "chevron.right")
            .font(.caption2.weight(.semibold))
            .imageScale(.small)
            .foregroundStyle(ejaTheme.inkMuted)
            .rotationEffect(.degrees(expanded ? 90 : 0))
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        TextField("", text: text, prompt: Text(placeholder).foregroundStyle(ejaTheme.inkMuted))
            .font(.system(.callout, design: .monospaced))
            .foregroundStyle(ejaTheme.ink)
            .textFieldStyle(.plain)
            .modifier(CardField())
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
        _ keyPath: WritableKeyPath<CapabilityTransportDTO, T?>,
        _ fallback: T
    ) -> Binding<T> {
        Binding(
            get: {
                guard draft.capabilities.indices.contains(index) else { return fallback }
                return draft.capabilities[index].transport[keyPath: keyPath] ?? fallback
            },
            set: {
                guard draft.capabilities.indices.contains(index) else { return }
                draft.capabilities[index].transport[keyPath: keyPath] = $0
            }
        )
    }

    private func commandLineBinding(_ index: Int) -> Binding<String> {
        Binding(
            get: {
                guard draft.capabilities.indices.contains(index) else { return "" }
                let cap = draft.capabilities[index]
                return draft.rawCommandLines[cap.id] ?? SettingsDraft.formatCommandLine(cap.transport)
            },
            set: { raw in
                guard draft.capabilities.indices.contains(index) else { return }
                let id = draft.capabilities[index].id
                draft.rawCommandLines[id] = raw
                let parsed = SettingsDraft.parseCommandLine(raw)
                draft.capabilities[index].transport.command = parsed.command
                draft.capabilities[index].transport.args = parsed.args
            }
        )
    }

    private func transportKVBinding(_ index: Int) -> Binding<String> {
        Binding(
            get: {
                guard draft.capabilities.indices.contains(index) else { return "" }
                let cap = draft.capabilities[index]
                return draft.rawTransportKVs[cap.id] ?? SettingsDraft.formatTransportKV(cap.transport)
            },
            set: { raw in
                guard draft.capabilities.indices.contains(index) else { return }
                let id = draft.capabilities[index].id
                draft.rawTransportKVs[id] = raw
                let map = SettingsDraft.parseTransportKV(raw)
                if draft.capabilities[index].transport.type == "http" {
                    draft.capabilities[index].transport.headers = map
                } else {
                    draft.capabilities[index].transport.env = map
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
                    .fill(ejaTheme.canvas)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .stroke(ejaTheme.hairline, lineWidth: 1)
            )
    }
}
