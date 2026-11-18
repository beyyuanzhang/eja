import SwiftUI

struct SettingsScreen: View {
    let app: AppModel
    @Bindable var model: SettingsModel
    let onClose: () -> Void
    @AppStorage("eja.settings.section") private var sectionRaw = SettingsSectionKind.model.rawValue
    @State private var saveError = ""

    enum SettingsSectionKind: String, CaseIterable, Identifiable {
        case model
        case capabilities
        case runtime
        case workspace

        var id: String { rawValue }

        var label: String {
            switch self {
            case .model: return "模型"
            case .capabilities: return "能力"
            case .runtime: return "运行"
            case .workspace: return "工作区"
            }
        }

        var icon: String {
            switch self {
            case .model: return "cpu"
            case .capabilities: return "puzzlepiece.extension"
            case .runtime: return "speedometer"
            case .workspace: return "folder"
            }
        }
    }

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider().overlay(Theme.line)
            VStack(spacing: 0) {
                header
                Divider().overlay(Theme.line)
                content
            }
            .background(Theme.paper)
        }
        .background(Theme.paper)
        .tint(Theme.accent)
        .task { await model.load(client: app.client, force: false) }
        .onExitCommand(perform: close)
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Button(action: close) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.inkSoft)
                        .frame(width: 24, height: 24)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 6))
                        .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Theme.line))
                }
                .buttonStyle(.plain)
                .help("返回 talk（esc）")
                .keyboardShortcut(.cancelAction)
                .disabled(model.saving)

                Text("设置")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.ink)
            }
            .padding(.horizontal, 14)
            .padding(.top, 12)
            .padding(.bottom, 20)

            VStack(spacing: 2) {
                ForEach(SettingsSectionKind.allCases) { kind in
                    SettingsNavRow(
                        kind: kind,
                        selected: section == kind
                    ) {
                        sectionRaw = kind.rawValue
                    }
                }
            }
            .padding(.horizontal, 8)

            Spacer(minLength: 20)
        }
        .frame(width: 188)
        .background(Theme.surface2.opacity(0.48))
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text(section.label)
                .font(.system(size: 16, weight: .semibold))
                .foregroundStyle(Theme.ink)

            Spacer()

            saveStatus

            if model.dirty || model.saving {
                Button {
                    save()
                } label: {
                    if model.saving {
                        ProgressView()
                            .controlSize(.mini)
                            .frame(width: 30)
                    } else {
                        Text("保存")
                            .frame(minWidth: 30)
                    }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .tint(Theme.accent)
                .disabled(model.saving || model.loading)
                .keyboardShortcut("s", modifiers: .command)
            }
        }
        .padding(.horizontal, 30)
        .frame(height: 54)
    }

    private var section: SettingsSectionKind {
        SettingsSectionKind(rawValue: sectionRaw) ?? .model
    }

    @ViewBuilder
    private var saveStatus: some View {
        if !saveError.isEmpty {
            Label(saveError, systemImage: "exclamationmark.circle")
                .font(.system(size: 11))
                .foregroundStyle(Theme.danger)
                .lineLimit(1)
        } else if model.savedFlash {
            Image(systemName: "checkmark")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Theme.accent)
                .help("已保存")
        }
    }

    @ViewBuilder
    private var content: some View {
        if model.loading && model.view == nil {
            VStack {
                Spacer()
                ProgressView()
                    .controlSize(.small)
                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if !model.loadError.isEmpty && model.view == nil {
            VStack(spacing: 12) {
                Spacer()
                Text(model.loadError)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.danger)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 30)
                Button("重试") {
                    Task { await model.load(client: app.client, force: true) }
                }
                .buttonStyle(.bordered)
                .tint(Theme.accent)
                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            sectionBody
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder
    private var sectionBody: some View {
        switch section {
        case .model:
            ProviderSection(app: app, model: model)
        case .capabilities:
            ConnectorSection(app: app, model: model)
        case .runtime:
            RuntimeSection(app: app, model: model)
        case .workspace:
            WorkspaceSection(model: model)
        }
    }

    private func save() {
        guard model.dirty, !model.saving else { return }
        saveError = ""
        Task {
            let succeeded = await model.save(client: app.client)
            if succeeded {
                app.refreshSoon()
            } else {
                saveError = "保存失败，请检查各项配置。"
                app.toast("设置保存失败，请检查各项配置。", isError: true)
            }
        }
    }

    private func close() {
        guard !model.saving else { return }
        onClose()
    }
}

private struct SettingsNavRow: View {
    let kind: SettingsScreen.SettingsSectionKind
    let selected: Bool
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 9) {
                Image(systemName: kind.icon)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(selected ? Theme.accent : Theme.muted)
                    .frame(width: 18)
                Text(kind.label)
                    .font(.system(size: 12.5, weight: selected ? .medium : .regular))
                    .foregroundStyle(selected ? Theme.ink : Theme.inkSoft)
                Spacer()
            }
            .padding(.horizontal, 9)
            .frame(height: 32)
            .background(
                selected
                    ? Theme.ink.opacity(0.07)
                    : (hovering ? Theme.ink.opacity(0.035) : .clear),
                in: RoundedRectangle(cornerRadius: 7)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}
