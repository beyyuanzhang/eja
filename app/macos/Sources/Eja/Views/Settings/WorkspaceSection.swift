import AppKit
import SwiftUI

struct WorkspaceSection: View {
    @Bindable var model: SettingsModel

    var body: some View {
        SettingsPage {
            SettingsGroup("默认") {
                SettingsRow("default") {
                    Text(model.view?.projectDir ?? "—")
                        .font(Theme.mono(10.5))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                }
            }

            SettingsGroup("额外目录") {
                VStack(spacing: 0) {
                    ForEach(Array(model.workspaces.enumerated()), id: \.element.id) { index, workspace in
                        if index > 0 {
                            SettingsRule()
                        }
                        VStack(alignment: .leading, spacing: 9) {
                            LabeledContent("名称") {
                                HStack(spacing: 8) {
                                    TextField("名称", text: textBinding(workspace.id, \.name))
                                        .labelsHidden()
                                        .textFieldStyle(.roundedBorder)
                                        .multilineTextAlignment(.leading)
                                        .frame(maxWidth: .infinity)
                                    SettingsDeleteButton {
                                        model.removeWorkspace(id: workspace.id)
                                    }
                                }
                            }
                            LabeledContent("目录") {
                                HStack(spacing: 8) {
                                    TextField("/path/to/dir", text: textBinding(workspace.id, \.path))
                                        .labelsHidden()
                                        .textFieldStyle(.roundedBorder)
                                        .font(Theme.mono(11))
                                        .multilineTextAlignment(.leading)
                                    Button {
                                        chooseDirectory(id: workspace.id)
                                    } label: {
                                        Image(systemName: "folder")
                                    }
                                    .buttonStyle(.bordered)
                                    .help("选择目录")
                                }
                            }
                        }
                        .padding(.horizontal, 16)
                        .padding(.vertical, 12)
                    }
                    if !model.workspaces.isEmpty {
                        SettingsRule()
                    }
                    SettingsAddButton(title: "添加目录") {
                        model.addWorkspace()
                    }
                    .padding(.horizontal, 16)
                    .padding(.vertical, 11)
                }
            }
        }
    }

    private func textBinding(
        _ id: UUID,
        _ keyPath: WritableKeyPath<WorkspaceDraft, String>
    ) -> Binding<String> {
        Binding(
            get: { model.workspaces.first(where: { $0.id == id })?[keyPath: keyPath] ?? "" },
            set: { next in
                guard let index = model.workspaces.firstIndex(where: { $0.id == id }) else { return }
                model.workspaces[index][keyPath: keyPath] = next
                model.markDirty()
            }
        )
    }

    private func chooseDirectory(id: UUID) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "选择"
        panel.begin { response in
            guard response == .OK, let url = panel.url else { return }
            guard let index = model.workspaces.firstIndex(where: { $0.id == id }) else { return }
            model.workspaces[index].path = url.path
            model.markDirty()
        }
    }
}
