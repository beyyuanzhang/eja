import SwiftUI

struct ConnectorSection: View {
    let app: AppModel
    @Bindable var model: SettingsModel

    var body: some View {
        SettingsPage {
            SettingsGroup("能力") {
                VStack(spacing: 0) {
                    capabilityRow("工作区", note: "允许任务使用文件夹", value: $model.capabilities.workspace)
                    SettingsRule()
                    capabilityRow("文件", note: "读取、写入和搜索文件", value: $model.capabilities.files)
                    SettingsRule()
                    capabilityRow("Shell", note: "运行终端命令", value: $model.capabilities.shell)
                    SettingsRule()
                    capabilityRow("Git", note: "建立隔离环境并合并结果", value: $model.capabilities.git)
                    SettingsRule()
                    capabilityRow("MCP", note: "使用下方连接器", value: $model.capabilities.mcp)
                    SettingsRule()
                    capabilityRow("插件", note: "允许 EJA 安装和更新插件", value: $model.capabilities.plugins)
                }
            }

            SettingsGroup("MCP") {
                if model.connectors.isEmpty {
                    SettingsAddButton(title: "添加连接器") {
                        model.addConnector()
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 20)
                } else {
                    VStack(spacing: 0) {
                        ForEach(Array(model.connectors.enumerated()), id: \.element.id) { index, connector in
                            if index > 0 {
                                SettingsRule()
                            }
                            ConnectorCard(app: app, connector: connectorBinding(for: connector)) {
                                model.removeConnector(id: connector.id)
                            } onChanged: {
                                model.markDirty()
                            }
                        }
                        SettingsRule()
                        SettingsAddButton(title: "添加连接器") {
                            model.addConnector()
                        }
                        .padding(.horizontal, 16)
                        .padding(.vertical, 11)
                    }
                }
            }
        }
    }

    private func capabilityRow(
        _ title: String,
        note: String,
        value: Binding<Bool>
    ) -> some View {
        SettingsRow(title, note: note) {
            Toggle("", isOn: value)
                .labelsHidden()
                .toggleStyle(.switch)
                .controlSize(.small)
                .tint(Theme.accent)
                .onChange(of: value.wrappedValue) { _, _ in model.markDirty() }
        }
    }

    private func connectorBinding(for connector: ConnectorDraft) -> Binding<ConnectorDraft> {
        Binding(
            get: { model.connectors.first(where: { $0.id == connector.id }) ?? connector },
            set: { next in
                guard let index = model.connectors.firstIndex(where: { $0.id == connector.id }) else { return }
                model.connectors[index] = next
                model.markDirty()
            }
        )
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
    @State private var expanded = true

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            VStack(alignment: .leading, spacing: 12) {
                Picker("模式", selection: $connector.mode) {
                    Text("本地命令").tag("command")
                    Text("远程 URL").tag("url")
                }
                .pickerStyle(.segmented)
                .onChange(of: connector.mode) { _, _ in draftChanged() }
                if connector.mode == "url" {
                    field("URL") {
                        TextField("https://service.example/mcp", text: $connector.url)
                            .textFieldStyle(.roundedBorder)
                            .font(Theme.mono(11.5))
                            .multilineTextAlignment(.leading)
                            .labelsHidden()
                            .onChange(of: connector.url) { _, _ in draftChanged() }
                    }
                    DisclosureGroup("Headers") {
                        KeyValueRowsEditor(rows: $connector.headers, kind: .headers, onChanged: draftChanged)
                            .padding(.vertical, 6)
                    }
                } else {
                    field("命令") {
                        TextField("/usr/local/bin/my-mcp-server", text: $connector.command)
                            .textFieldStyle(.roundedBorder)
                            .font(Theme.mono(11.5))
                            .multilineTextAlignment(.leading)
                            .labelsHidden()
                            .onChange(of: connector.command) { _, _ in draftChanged() }
                    }
                    VStack(alignment: .leading, spacing: 6) {
                        SettingsFieldLabel(text: "参数")
                        ForEach(Array(connector.args.enumerated()), id: \.offset) { index, _ in
                            HStack(spacing: 6) {
                                TextField("--stdio", text: argRow(index))
                                    .textFieldStyle(.roundedBorder)
                                    .font(Theme.mono(11))
                                    .multilineTextAlignment(.leading)
                                    .labelsHidden()
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
                            .textFieldStyle(.roundedBorder)
                            .font(Theme.mono(11.5))
                            .multilineTextAlignment(.leading)
                            .labelsHidden()
                            .onChange(of: connector.cwd) { _, _ in draftChanged() }
                    }
                    DisclosureGroup("环境变量") {
                        KeyValueRowsEditor(rows: $connector.env, kind: .env, onChanged: draftChanged)
                            .padding(.vertical, 6)
                    }
                }
                LabeledContent("超时 (ms)") {
                    HStack(spacing: 8) {
                        TextField("30000", text: $timeoutText)
                            .textFieldStyle(.roundedBorder)
                            .font(Theme.mono(11.5))
                            .frame(width: 100)
                            .labelsHidden()
                            .onChange(of: timeoutText) { _, next in
                                if let parsed = Int(next.trimmingCharacters(in: .whitespaces)), parsed >= 0 {
                                    connector.timeoutMs = parsed
                                    draftChanged()
                                }
                            }
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
                        }
                        .buttonStyle(.borderless)
                        .foregroundStyle(Theme.accent)
                        .disabled(testing)
                    }
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
            .padding(.top, 5)
        } label: {
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
                    .multilineTextAlignment(.leading)
                    .labelsHidden()
                    .onChange(of: connector.name) { _, _ in draftChanged() }
                Toggle("", isOn: $connector.enabled)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
                    .tint(Theme.accent)
                    .onChange(of: connector.enabled) { _, _ in draftChanged() }
                SettingsDeleteButton(action: onDelete)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .opacity(connector.enabled ? 1 : 0.72)
        .onAppear { timeoutText = String(connector.timeoutMs) }
    }

    private func field(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            SettingsFieldLabel(text: label)
            content()
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
