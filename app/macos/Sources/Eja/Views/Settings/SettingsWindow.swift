import SwiftUI

struct SettingsWindow: View {
    let app: AppModel
    @State private var model = SettingsModel()
    @State private var section: SettingsSectionKind = .model
    @State private var saveError = ""

    enum SettingsSectionKind: String, CaseIterable, Identifiable {
        case model
        case connectors
        case runtime
        case workspace

        var id: String { rawValue }

        var label: String {
            switch self {
            case .model: return "模型"
            case .connectors: return "连接器"
            case .runtime: return "运行"
            case .workspace: return "工作区"
            }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider().overlay(Theme.line)
            content
            Divider().overlay(Theme.line)
            saveBar
        }
        .frame(width: 640, height: 560)
        .background(Theme.paper)
        .tint(Theme.accent)
        .task { await model.load(client: app.client, force: false) }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("设置")
                .font(Theme.serif(21, weight: .medium))
                .foregroundStyle(Theme.ink)
            Picker("", selection: $section) {
                ForEach(SettingsSectionKind.allCases) { kind in
                    Text(kind.label).tag(kind)
                }
            }
            .labelsHidden()
            .pickerStyle(.segmented)
        }
        .padding(.horizontal, 22)
        .padding(.top, 18)
        .padding(.bottom, 14)
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
            ScrollView {
                sectionBody
                    .padding(.horizontal, 22)
                    .padding(.vertical, 18)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    @ViewBuilder
    private var sectionBody: some View {
        switch section {
        case .model:
            ProviderSection(app: app, model: model)
        case .connectors:
            ConnectorSection(app: app, model: model)
        case .runtime:
            RuntimeSection(app: app, model: model)
        case .workspace:
            WorkspaceSection(model: model)
        }
    }

    private var saveBar: some View {
        HStack(spacing: 10) {
            if model.saving {
                ProgressView()
                    .controlSize(.mini)
            } else if model.savedFlash {
                Text("已保存")
                    .font(.system(size: 11.5))
                    .foregroundStyle(Theme.accent)
            } else if !saveError.isEmpty {
                Text(saveError)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(1)
                    .truncationMode(.tail)
            } else if model.dirty {
                HStack(spacing: 6) {
                    Circle()
                        .fill(Theme.accent)
                        .frame(width: 5, height: 5)
                    Text("有未保存的修改")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.muted)
                }
            }
            Spacer()
            Button {
                save()
            } label: {
                Text("保存")
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(model.dirty && !model.saving ? Color.white : Theme.faint)
                    .padding(.horizontal, 18)
                    .padding(.vertical, 7)
                    .background(
                        model.dirty && !model.saving ? Theme.accent : Theme.surface2,
                        in: RoundedRectangle(cornerRadius: 8)
                    )
            }
            .buttonStyle(.plain)
            .disabled(!model.dirty || model.saving || model.loading)
            .keyboardShortcut("s", modifiers: .command)
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 12)
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
}
