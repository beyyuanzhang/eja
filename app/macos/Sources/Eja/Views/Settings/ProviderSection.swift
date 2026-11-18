import SwiftUI

struct ProviderSection: View {
    let app: AppModel
    @Bindable var model: SettingsModel

    var body: some View {
        SettingsPage {
            SettingsGroup("Provider") {
                VStack(spacing: 0) {
                    if model.providers.isEmpty {
                        Text("还没有 Provider。添加一个 OpenAI 兼容端点，再在下面绑定角色。")
                            .font(.system(size: 12))
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(16)
                    } else {
                        ForEach(Array(model.providers.enumerated()), id: \.element.id) { index, provider in
                            if index > 0 {
                                SettingsRule()
                            }
                            ProviderCard(app: app, provider: providerBinding(for: provider)) {
                                model.removeProvider(id: provider.id)
                            } onChanged: {
                                model.markDirty()
                            }
                        }
                    }
                    SettingsRule()
                    SettingsAddButton(title: "添加 Provider") {
                        model.addProvider()
                    }
                    .padding(.horizontal, 16)
                    .padding(.vertical, 11)
                }
            }

            SettingsGroup("角色") {
                VStack(spacing: 0) {
                    RoleBindingRow(
                        title: "eja",
                        binding: $model.roles.eja,
                        providers: model.providers,
                        emptyProviderLabel: "Provider",
                        emptyModelLabel: "模型",
                        onChanged: { model.markDirty() }
                    )
                    SettingsRule()
                    RoleBindingRow(
                        title: "AJA",
                        binding: $model.roles.aja,
                        providers: model.providers,
                        emptyProviderLabel: "跟随 eja",
                        emptyModelLabel: model.roles.aja.providerId.isEmpty ? "跟随 eja" : "模型",
                        onChanged: { model.markDirty() }
                    )
                }
            }
        }
    }

    private func providerBinding(for provider: ProviderDraft) -> Binding<ProviderDraft> {
        Binding(
            get: { model.providers.first(where: { $0.id == provider.id }) ?? provider },
            set: { next in
                guard let index = model.providers.firstIndex(where: { $0.id == provider.id }) else { return }
                model.providers[index] = next
                model.markDirty()
            }
        )
    }
}

private struct RoleBindingRow: View {
    let title: String
    @Binding var binding: RoleBinding
    let providers: [ProviderDraft]
    let emptyProviderLabel: String
    let emptyModelLabel: String
    let onChanged: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Text(title)
                .font(Theme.mono(11.5))
                .foregroundStyle(Theme.inkSoft)
                .frame(width: 38, alignment: .leading)
            Picker("", selection: providerSelection) {
                Text(emptyProviderLabel).tag("")
                ForEach(providers) { provider in
                    Text(providerDisplayName(provider)).tag(provider.id)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .tint(Theme.inkSoft)
            .frame(minWidth: 120, idealWidth: 170, maxWidth: 210, alignment: .leading)
            modelControl
                .frame(minWidth: 120, idealWidth: 190, maxWidth: 230, alignment: .leading)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var providerSelection: Binding<String> {
        Binding(
            get: { binding.providerId },
            set: { nextId in
                let provider = providers.first { $0.id == nextId }
                let keepModel = provider.map { $0.models.contains(binding.model) } ?? false
                let soleModel = provider?.models.count == 1 ? provider?.models.first : nil
                binding = RoleBinding(
                    providerId: nextId,
                    model: keepModel ? binding.model : (soleModel ?? "")
                )
                onChanged()
            }
        )
    }

    @ViewBuilder
    private var modelControl: some View {
        if let provider = providers.first(where: { $0.id == binding.providerId }), !provider.models.isEmpty {
            Picker("", selection: $binding.model) {
                Text(emptyModelLabel).tag("")
                ForEach(modelOptions(provider), id: \.self) { name in
                    Text(name).tag(name)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .tint(Theme.inkSoft)
            .onChange(of: binding.model) { _, _ in onChanged() }
        } else {
            TextField(emptyModelLabel, text: $binding.model)
                .textFieldStyle(.roundedBorder)
                .font(Theme.mono(11.5))
                .multilineTextAlignment(.leading)
                .labelsHidden()
                .disabled(binding.providerId.isEmpty)
                .onChange(of: binding.model) { _, _ in onChanged() }
        }
    }

    private func modelOptions(_ provider: ProviderDraft) -> [String] {
        if !binding.model.isEmpty && !provider.models.contains(binding.model) {
            return [binding.model] + provider.models
        }
        return provider.models
    }

    private func providerDisplayName(_ provider: ProviderDraft) -> String {
        let trimmed = provider.name.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? provider.id : trimmed
    }
}

private struct ProviderCard: View {
    let app: AppModel
    @Binding var provider: ProviderDraft
    let onDelete: () -> Void
    let onChanged: () -> Void
    @State private var discovering = false
    @State private var discoverMessage = ""
    @State private var discoverFailed = false
    @State private var expanded = true

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 12) {
                LabeledContent("Base URL") {
                    TextField("https://api.example.com/v1", text: $provider.baseUrl)
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.mono(11.5))
                        .multilineTextAlignment(.leading)
                        .labelsHidden()
                        .onChange(of: provider.baseUrl) { _, _ in modelChanged() }
                }
                LabeledContent("API key") {
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 6) {
                            SecureField(keyPlaceholder, text: $provider.apiKey)
                                .textFieldStyle(.roundedBorder)
                                .font(Theme.mono(11.5))
                                .multilineTextAlignment(.leading)
                                .labelsHidden()
                                .disabled(provider.clearApiKey)
                                .onChange(of: provider.apiKey) { _, newKey in
                                    if !newKey.isEmpty { provider.clearApiKey = false }
                                    modelChanged()
                                }
                            if provider.hasApiKey {
                                Button {
                                    provider.clearApiKey.toggle()
                                    if provider.clearApiKey {
                                        provider.apiKey = ""
                                    }
                                    modelChanged()
                                } label: {
                                    Image(systemName: provider.clearApiKey ? "arrow.uturn.backward.circle" : "xmark.circle")
                                        .font(.system(size: 11.5))
                                        .foregroundStyle(provider.clearApiKey ? Theme.accent : Theme.faint)
                                }
                                .buttonStyle(.borderless)
                                .help(provider.clearApiKey ? "保留已保存密钥" : "清除已保存密钥")
                            }
                        }
                    }
                }
                DisclosureGroup("Headers") {
                    KeyValueRowsEditor(rows: $provider.headers, kind: .headers, onChanged: modelChanged)
                        .padding(.vertical, 6)
                }
                DisclosureGroup("模型") {
                    VStack(alignment: .leading, spacing: 6) {
                        ForEach(Array(provider.models.enumerated()), id: \.offset) { index, _ in
                            HStack(spacing: 6) {
                                TextField("model-name", text: modelRow(index))
                                    .textFieldStyle(.roundedBorder)
                                    .font(Theme.mono(11))
                                    .multilineTextAlignment(.leading)
                                    .labelsHidden()
                                    .onChange(of: provider.models[index]) { _, _ in modelChanged() }
                                SettingsDeleteButton {
                                    guard provider.models.indices.contains(index) else { return }
                                    provider.models.remove(at: index)
                                    modelChanged()
                                }
                            }
                        }
                        SettingsAddButton(title: "添加模型") {
                            provider.models.append("")
                            modelChanged()
                        }
                        HStack {
                            Button {
                                discover()
                            } label: {
                                HStack(spacing: 5) {
                                    if discovering {
                                        ProgressView().controlSize(.mini)
                                    } else {
                                        Image(systemName: "arrow.triangle.2.circlepath")
                                    }
                                    Text("发现模型")
                                }
                            }
                            .buttonStyle(.borderless)
                            .foregroundStyle(Theme.accent)
                            .disabled(discovering || provider.baseUrl.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                            if !discoverMessage.isEmpty {
                                Text(discoverMessage)
                                    .font(.system(size: 11))
                                    .foregroundStyle(discoverFailed ? Theme.danger : .secondary)
                                    .textSelection(.enabled)
                            }
                        }
                    }
                    .padding(.vertical, 6)
                }
            }
            .padding(.top, 5)
        } label: {
            HStack(spacing: 8) {
                Text(mark)
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(provider.local ? Theme.accent : Theme.muted)
                    .frame(width: 26, height: 26)
                    .background(provider.local ? Theme.accentWash : Theme.surface2, in: RoundedRectangle(cornerRadius: 8))
                TextField("名称", text: $provider.name)
                    .textFieldStyle(.plain)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .multilineTextAlignment(.leading)
                    .labelsHidden()
                    .onChange(of: provider.name) { _, _ in modelChanged() }
                if provider.local {
                    Text("本地")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.accent)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Theme.accentWash, in: Capsule())
                }
                Spacer()
                SettingsDeleteButton(action: onDelete)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var mark: String {
        let trimmed = provider.name.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? "P" : String(trimmed.prefix(1)).uppercased()
    }

    private var keyPlaceholder: String {
        if provider.clearApiKey { return "保存时清除" }
        if provider.hasApiKey { return "已保存密钥" }
        return provider.local ? "可选" : "sk-…"
    }

    private func modelRow(_ index: Int) -> Binding<String> {
        Binding(
            get: { provider.models.indices.contains(index) ? provider.models[index] : "" },
            set: { next in
                guard provider.models.indices.contains(index) else { return }
                provider.models[index] = next
            }
        )
    }

    private func modelChanged() {
        discoverMessage = ""
        discoverFailed = false
        onChanged()
    }

    private func discover() {
        guard !discovering else { return }
        discovering = true
        discoverMessage = ""
        let trimmedKey = provider.apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        let body = DiscoverBody(
            id: provider.id,
            baseUrl: provider.baseUrl.trimmingCharacters(in: .whitespacesAndNewlines),
            apiKey: provider.clearApiKey ? "" : (trimmedKey.isEmpty ? nil : trimmedKey),
            headers: provider.headers
        )
        Task {
            defer { discovering = false }
            do {
                let result = try await app.client.discover(body)
                if result.ok, let models = result.models {
                    provider.models = models
                    discoverFailed = false
                    discoverMessage = "发现 \(result.total ?? models.count) 个模型 · \(result.latencyMs) ms"
                    onChanged()
                } else {
                    discoverFailed = true
                    discoverMessage = friendlyError(result.error ?? "发现失败")
                }
            } catch {
                discoverFailed = true
                discoverMessage = friendlyError(error.localizedDescription)
            }
        }
    }
}
