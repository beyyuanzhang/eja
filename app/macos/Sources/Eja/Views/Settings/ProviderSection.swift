import SwiftUI

struct ProviderSection: View {
    let app: AppModel
    @Bindable var model: SettingsModel

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    SettingsFieldLabel(text: "PROVIDER")
                    Spacer()
                }
                if model.providers.isEmpty {
                    Text("还没有 Provider。添加一个 OpenAI 兼容端点，再在下面绑定角色。")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.muted)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .card()
                } else {
                    ForEach($model.providers) { $provider in
                        ProviderCard(app: app, provider: $provider) {
                            deleteProvider(id: provider.id)
                        } onChanged: {
                            model.markDirty()
                        }
                    }
                }
                SettingsAddButton(title: "添加 Provider") {
                    model.providers.append(ProviderDraft())
                    model.markDirty()
                }
            }
            VStack(alignment: .leading, spacing: 8) {
                SettingsFieldLabel(text: "角色")
                VStack(alignment: .leading, spacing: 12) {
                    RoleBindingRow(
                        title: "eja",
                        binding: $model.roles.eja,
                        providers: model.providers,
                        emptyProviderLabel: "Provider",
                        emptyModelLabel: "模型",
                        onChanged: { model.markDirty() }
                    )
                    Divider().overlay(Theme.line)
                    RoleBindingRow(
                        title: "AJA",
                        binding: $model.roles.aja,
                        providers: model.providers,
                        emptyProviderLabel: "跟随 eja",
                        emptyModelLabel: model.roles.aja.providerId.isEmpty ? "跟随 eja" : "模型",
                        onChanged: { model.markDirty() }
                    )
                }
                .card()
            }
        }
    }

    private func deleteProvider(id: String) {
        model.providers.removeAll { $0.id == id }
        if model.roles.eja.providerId == id { model.roles.eja = .empty }
        if model.roles.aja.providerId == id { model.roles.aja = .empty }
        model.markDirty()
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
            .frame(maxWidth: .infinity, alignment: .leading)
            modelControl
                .frame(maxWidth: .infinity, alignment: .leading)
        }
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
                .textFieldStyle(.plain)
                .font(Theme.mono(11.5))
                .foregroundStyle(Theme.ink)
                .padding(.horizontal, 8)
                .frame(height: 26)
                .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
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

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
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
                    .onChange(of: provider.name) { _, _ in modelChanged() }
                if provider.local {
                    Text("本地")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.accent)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Theme.accentWash, in: Capsule())
                }
                SettingsDeleteButton(action: onDelete)
            }
            field("Base URL") {
                TextField("https://api.example.com/v1", text: $provider.baseUrl)
                    .textFieldStyle(.plain)
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.ink)
                    .onChange(of: provider.baseUrl) { _, _ in modelChanged() }
            }
            field("API key") {
                VStack(alignment: .leading, spacing: 6) {
                    SecureField(keyPlaceholder, text: $provider.apiKey)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11.5))
                        .foregroundStyle(Theme.ink)
                        .disabled(provider.clearApiKey)
                        .onChange(of: provider.apiKey) { _, newKey in
                            if !newKey.isEmpty { provider.clearApiKey = false }
                            modelChanged()
                        }
                    if provider.hasApiKey {
                        Toggle(isOn: $provider.clearApiKey) {
                            Text("清除密钥")
                                .font(.system(size: 11))
                                .foregroundStyle(Theme.muted)
                        }
                        .toggleStyle(.checkbox)
                        .tint(Theme.accent)
                        .onChange(of: provider.clearApiKey) { _, cleared in
                            if cleared { provider.apiKey = "" }
                            modelChanged()
                        }
                    }
                }
            }
            VStack(alignment: .leading, spacing: 6) {
                SettingsFieldLabel(text: "HEADERS")
                KeyValueRowsEditor(rows: $provider.headers, kind: .headers, onChanged: modelChanged)
            }
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    SettingsFieldLabel(text: "模型")
                    Spacer()
                    Button {
                        discover()
                    } label: {
                        HStack(spacing: 5) {
                            if discovering {
                                ProgressView().controlSize(.mini)
                            } else {
                                Image(systemName: "arrow.triangle.2.circlepath")
                                    .font(.system(size: 10.5, weight: .semibold))
                            }
                            Text("发现模型")
                                .font(.system(size: 11.5))
                        }
                        .foregroundStyle(Theme.accent)
                        .padding(.horizontal, 9)
                        .padding(.vertical, 5)
                        .background(Theme.accentWash, in: RoundedRectangle(cornerRadius: 7))
                    }
                    .buttonStyle(.plain)
                    .disabled(discovering || provider.baseUrl.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
                ForEach(Array(provider.models.enumerated()), id: \.offset) { index, _ in
                    HStack(spacing: 6) {
                        TextField("model-name", text: modelRow(index))
                            .textFieldStyle(.plain)
                            .font(Theme.mono(11))
                            .foregroundStyle(Theme.ink)
                            .padding(.horizontal, 8)
                            .frame(height: 28)
                            .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                            .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
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
                if !discoverMessage.isEmpty {
                    Text(discoverMessage)
                        .font(.system(size: 11))
                        .foregroundStyle(discoverFailed ? Theme.danger : Theme.accent)
                        .textSelection(.enabled)
                }
            }
        }
        .card()
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

    private func field(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            SettingsFieldLabel(text: label)
            content()
                .padding(.horizontal, 8)
                .padding(.vertical, 6)
                .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
        }
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
