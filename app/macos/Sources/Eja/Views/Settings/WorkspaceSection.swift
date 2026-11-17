import AppKit
import SwiftUI

struct WorkspaceSection: View {
    @Bindable var model: SettingsModel

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("默认工作区是项目目录，不在下列表中；这里添加额外授权的目录。")
                .font(.system(size: 11))
                .foregroundStyle(Theme.muted)
            HStack(spacing: 8) {
                Text("default")
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.inkSoft)
                    .frame(width: 120, alignment: .leading)
                Text(model.view?.projectDir ?? "—")
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.muted)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .textSelection(.enabled)
            }
            .card(padding: 12)
            ForEach($model.workspaces) { $workspace in
                HStack(spacing: 6) {
                    TextField("name", text: $workspace.name)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.ink)
                        .padding(.horizontal, 8)
                        .frame(height: 28)
                        .frame(width: 120)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
                        .onChange(of: workspace.name) { _, _ in model.markDirty() }
                    TextField("/path/to/dir", text: $workspace.path)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.ink)
                        .padding(.horizontal, 8)
                        .frame(height: 28)
                        .frame(maxWidth: .infinity)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
                        .onChange(of: workspace.path) { _, _ in model.markDirty() }
                    Button("选择…") {
                        chooseDirectory(for: $workspace.path)
                    }
                    .buttonStyle(.plain)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.muted)
                    .padding(.horizontal, 8)
                    .frame(height: 28)
                    .background(Theme.surface2, in: RoundedRectangle(cornerRadius: 7))
                    SettingsDeleteButton {
                        model.workspaces.removeAll { $0.id == workspace.id }
                        model.markDirty()
                    }
                }
            }
            SettingsAddButton(title: "添加工作区") {
                model.workspaces.append(WorkspaceDraft(name: "", path: ""))
                model.markDirty()
            }
        }
    }

    private func chooseDirectory(for path: Binding<String>) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "选择"
        panel.begin { response in
            guard response == .OK, let url = panel.url else { return }
            path.wrappedValue = url.path
            model.markDirty()
        }
    }
}
