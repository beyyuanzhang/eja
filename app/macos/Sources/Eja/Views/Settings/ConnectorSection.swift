import SwiftUI

struct ConnectorSection: View {
    let app: AppModel
    @Bindable var model: SettingsModel

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let error = model.view?.connectors.error, !error.isEmpty {
                Text(error)
                    .font(.system(size: 11.5))
                    .foregroundStyle(Theme.danger)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
                    .background(Theme.dangerWash, in: RoundedRectangle(cornerRadius: 9))
            }
            HStack(spacing: 10) {
                Image(systemName: "folder")
                    .font(.system(size: 13))
                    .foregroundStyle(Theme.accent)
                    .frame(width: 26, height: 26)
                    .background(Theme.accentWash, in: RoundedRectangle(cornerRadius: 8))
                VStack(alignment: .leading, spacing: 1) {
                    Text("workspace")
                        .font(.system(size: 12.5, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                    Text("内置工作区工具")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.muted)
                }
                Spacer()
                Toggle("", isOn: $model.workspaceTools)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .tint(Theme.accent)
                    .onChange(of: model.workspaceTools) { _, _ in model.markDirty() }
            }
            .card(padding: 12)
            ForEach($model.connectors) { $connector in
                ConnectorCard(app: app, connector: $connector) {
                    model.connectors.removeAll { $0.id == connector.id }
                    model.markDirty()
                } onChanged: {
                    model.markDirty()
                }
            }
            SettingsAddButton(title: "添加连接器") {
                model.connectors.append(ConnectorDraft())
                model.markDirty()
            }
        }
    }
}

private struct ConnectorCard: View {
    let app: AppModel
    @Binding var connector: ConnectorDraft
    let onDelete: () -> Void
    let onChanged: () -> Void
    @State private var timeoutText = ""
    @State private var testing = false
    @State private var testMessage = ""
    @State private var testFailed = false
    @State private var testStderr = ""
    @State private var testTools: [ConnectorTool] = []

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: connector.mode == "url" ? "globe" : "terminal")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.muted)
                    .frame(width: 26, height: 26)
                    .background(Theme.surface2, in: RoundedRectangle(cornerRadius: 8))
                TextField("名称", text: $connector.name)
                    .textFieldStyle(.plain)
                    .font(Theme.mono(12.5))
                    .foregroundStyle(Theme.ink)
                    .onChange(of: connector.name) { _, _ in draftChanged() }
                Toggle("", isOn: $connector.enabled)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .tint(Theme.accent)
                    .onChange(of: connector.enabled) { _, _ in draftChanged() }
                SettingsDeleteButton(action: onDelete)
            }
            Picker("", selection: $connector.mode) {
                Text("本地命令").tag("command")
                Text("远程 URL").tag("url")
            }
            .labelsHidden()
            .pickerStyle(.segmented)
            .onChange(of: connector.mode) { _, _ in draftChanged() }
            if connector.mode == "url" {
                field("URL") {
                    TextField("https://service.example/mcp", text: $connector.url)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11.5))
                        .foregroundStyle(Theme.ink)
                        .onChange(of: connector.url) { _, _ in draftChanged() }
                }
                VStack(alignment: .leading, spacing: 6) {
                    SettingsFieldLabel(text: "HEADERS")
                    KeyValueRowsEditor(rows: $connector.headers, kind: .headers, onChanged: draftChanged)
                }
            } else {
                field("命令") {
                    TextField("/usr/local/bin/my-mcp-server", text: $connector.command)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11.5))
                        .foregroundStyle(Theme.ink)
                        .onChange(of: connector.command) { _, _ in draftChanged() }
                }
                VStack(alignment: .leading, spacing: 6) {
                    SettingsFieldLabel(text: "参数")
                    ForEach(Array(connector.args.enumerated()), id: \.offset) { index, _ in
                        HStack(spacing: 6) {
                            TextField("--stdio", text: argRow(index))
                                .textFieldStyle(.plain)
                                .font(Theme.mono(11))
                                .foregroundStyle(Theme.ink)
                                .padding(.horizontal, 8)
                                .frame(height: 28)
                                .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
                                .onChange(of: connector.args[index]) { _, _ in draftChanged() }
                            SettingsDeleteButton {
                                guard connector.args.indices.contains(index) else { return }
                                connector.args.remove(at: index)
                                draftChanged()
                            }
                        }
                    }
                    SettingsAddButton(title: "添加参数") {
                        connector.args.append("")
                        draftChanged()
                    }
                }
                field("工作目录") {
                    TextField("默认项目目录", text: $connector.cwd)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11.5))
                        .foregroundStyle(Theme.ink)
                        .onChange(of: connector.cwd) { _, _ in draftChanged() }
                }
                VStack(alignment: .leading, spacing: 6) {
                    SettingsFieldLabel(text: "环境变量")
                    KeyValueRowsEditor(rows: $connector.env, kind: .env, onChanged: draftChanged)
                }
            }
            HStack(spacing: 8) {
                SettingsFieldLabel(text: "超时 (ms)")
                TextField("30000", text: $timeoutText)
                    .textFieldStyle(.plain)
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 90)
                    .padding(.horizontal, 8)
                    .frame(height: 26)
                    .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                    .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
                    .onChange(of: timeoutText) { _, next in
                        if let parsed = Int(next.trimmingCharacters(in: .whitespaces)), parsed >= 0 {
                            connector.timeoutMs = parsed
                            draftChanged()
                        }
                    }
                Spacer()
                Button {
                    test()
                } label: {
                    HStack(spacing: 5) {
                        if testing {
                            ProgressView().controlSize(.mini)
                        } else {
                            Image(systemName: "bolt.horizontal")
                                .font(.system(size: 10.5, weight: .semibold))
                        }
                        Text("测试连接")
                            .font(.system(size: 11.5))
                    }
                    .foregroundStyle(Theme.accent)
                    .padding(.horizontal, 9)
                    .padding(.vertical, 5)
                    .background(Theme.accentWash, in: RoundedRectangle(cornerRadius: 7))
                }
                .buttonStyle(.plain)
                .disabled(testing)
            }
            if !testMessage.isEmpty {
                Text(testMessage)
                    .font(.system(size: 11))
                    .foregroundStyle(testFailed ? Theme.danger : Theme.accent)
                    .textSelection(.enabled)
            }
            if !testTools.isEmpty {
                Text(testTools.map { $0.publicName.isEmpty ? $0.name : $0.publicName }.joined(separator: " · "))
                    .font(Theme.mono(10))
                    .foregroundStyle(Theme.muted)
                    .textSelection(.enabled)
            }
            if !testStderr.isEmpty {
                ScrollView {
                    Text(testStderr)
                        .font(Theme.mono(10))
                        .foregroundStyle(Theme.muted)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                }
                .frame(maxHeight: 72)
                .padding(8)
                .background(Theme.surface2, in: RoundedRectangle(cornerRadius: 7))
            }
        }
        .card()
        .opacity(connector.enabled ? 1 : 0.72)
        .onAppear { timeoutText = String(connector.timeoutMs) }
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

    private func argRow(_ index: Int) -> Binding<String> {
        Binding(
            get: { connector.args.indices.contains(index) ? connector.args[index] : "" },
            set: { next in
                guard connector.args.indices.contains(index) else { return }
                connector.args[index] = next
            }
        )
    }

    private func draftChanged() {
        testMessage = ""
        testFailed = false
        testStderr = ""
        testTools = []
        onChanged()
    }

    private func test() {
        guard !testing else { return }
        testing = true
        testMessage = ""
        testStderr = ""
        testTools = []
        var probe = connector
        let trimmedName = probe.name.trimmingCharacters(in: .whitespacesAndNewlines)
        probe.name = trimmedName.isEmpty ? "probe" : trimmedName
        Task {
            defer { testing = false }
            do {
                let result = try await app.client.testConnector(probe)
                testFailed = !result.ok
                if result.ok {
                    let count = result.count ?? result.tools?.count ?? 0
                    testMessage = "连接成功 · \(count) 个工具 · \(result.latencyMs) ms"
                    testTools = result.tools ?? []
                } else {
                    testMessage = friendlyError(result.error ?? "测试失败")
                }
                testStderr = result.stderr ?? ""
            } catch {
                testFailed = true
                testMessage = friendlyError(error.localizedDescription)
            }
        }
    }
}
