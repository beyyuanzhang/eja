import AppKit
import EjaClient
import SwiftUI

@MainActor
final class SettingsDraft: ObservableObject {
    @Published var providers: [ProviderConfigDTO] = []
    @Published var judgeProviderId: String = ""
    @Published var judgeModel: String = ""
    @Published var selectedProviderId: String?

    @Published var connectors: [ConnectorConfigDTO] = []
    @Published var selectedConnectorId: String?
    @Published var activeTab: Int = 0

    func load(from config: HostConfigDTO) {
        providers = config.providers
        judgeProviderId = config.judge.providerId
        judgeModel = config.judge.model
        selectedProviderId = providers.first?.id
        connectors = config.connectors
        selectedConnectorId = connectors.first?.id
    }

    func buildConfig() -> HostConfigDTO {
        let judge = ModelRefDTO(providerId: judgeProviderId, model: judgeModel.trimmingCharacters(in: .whitespacesAndNewlines))
        return HostConfigDTO(
            providers: providers,
            judge: judge,
            connectors: connectors
        )
    }

    func addPreset(id: String, name: String, kind: String, baseUrl: String, models: [String]) {
        var uniqueId = id
        var n = 2
        while providers.contains(where: { $0.id == uniqueId }) {
            uniqueId = "\(id)-\(n)"
            n += 1
        }
        let item = ProviderConfigDTO(id: uniqueId, name: name, kind: kind, baseUrl: baseUrl, apiKey: "", models: models)
        providers.append(item)
        selectedProviderId = uniqueId
        if judgeProviderId.isEmpty {
            judgeProviderId = uniqueId
            judgeModel = models.first ?? ""
        }
    }

    func addConnector(type: String) {
        var uniqueId = "mcp-\(connectors.count + 1)"
        var n = connectors.count + 2
        while connectors.contains(where: { $0.id == uniqueId }) {
            uniqueId = "mcp-\(n)"
            n += 1
        }
        let transport: ConnectorTransportDTO = type == "http"
            ? .http(url: "http://127.0.0.1:8080/mcp")
            : .stdio(command: "/usr/bin/python3", args: [])
        let item = ConnectorConfigDTO(id: uniqueId, name: "新连接器", enabled: true, transport: transport)
        connectors.append(item)
        selectedConnectorId = uniqueId
    }
}

struct SettingsView: View {
    @ObservedObject var client: HostClient
    @Environment(\.dismiss) private var dismiss
    @StateObject private var draft = SettingsDraft()
    @Namespace private var tabNamespace
    @Namespace private var protocolNamespace
    @FocusState private var focusedField: String?

    var body: some View {
        VStack(spacing: 0) {
            headerBar
            hairlineHorizontal

            Group {
                if draft.activeTab == 0 {
                    llmTab
                } else {
                    connectorsTab
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            hairlineHorizontal
            footerBar
        }
        .background(EjaTheme.canvas)
        .tint(EjaTheme.ink)
        .frame(width: 620, height: 480)
        .onAppear { draft.load(from: client.config) }
    }

    private var headerBar: some View {
        HStack(spacing: 20) {
            tabButton(title: "模型", tag: 0)
            tabButton(title: "连接器", tag: 1)
            Spacer()
        }
        .padding(.horizontal, 18)
        .frame(height: 44)
    }

    private func tabButton(title: String, tag: Int) -> some View {
        let active = draft.activeTab == tag
        return Button {
            withAnimation(.easeOut(duration: 0.2)) {
                draft.activeTab = tag
            }
        } label: {
            VStack(spacing: 0) {
                Spacer(minLength: 0)
                Text(title)
                    .font(.system(size: 13, weight: active ? .medium : .regular))
                    .foregroundStyle(active ? EjaTheme.ink : EjaTheme.inkSecondary)
                Spacer(minLength: 0)
                ZStack {
                    Color.clear.frame(height: 1.5)
                    if active {
                        Rectangle()
                            .fill(EjaTheme.ink)
                            .frame(height: 1.5)
                            .matchedGeometryEffect(id: "settings-tab-line", in: tabNamespace)
                    }
                }
            }
            .fixedSize(horizontal: true, vertical: false)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var footerBar: some View {
        HStack(spacing: 8) {
            if !client.dataDir.isEmpty {
                Button {
                    NSWorkspace.shared.open(URL(fileURLWithPath: client.dataDir, isDirectory: true))
                } label: {
                    HStack(spacing: 5) {
                        Image(systemName: "folder")
                            .font(.system(size: 11))
                        Text("数据目录")
                    }
                }
                .buttonStyle(EjaSecondaryButtonStyle())
            }
            Spacer()
            Button("取消") { dismiss() }
                .buttonStyle(EjaSecondaryButtonStyle())
                .keyboardShortcut(.cancelAction)
            Button("保存") {
                client.saveConfig(draft.buildConfig())
                dismiss()
            }
            .buttonStyle(EjaPrimaryButtonStyle())
            .keyboardShortcut(.defaultAction)
        }
        .padding(.horizontal, 16)
        .frame(height: 48)
    }

    private var llmTab: some View {
        let activeJudgeProvider = draft.providers.first(where: { $0.id == draft.judgeProviderId })
        return VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text("主模型")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(EjaTheme.inkSecondary)
                    .frame(width: 52, alignment: .leading)

                Menu {
                    ForEach(draft.providers) { p in
                        Button(p.name) {
                            draft.judgeProviderId = p.id
                            if draft.judgeModel.trimmingCharacters(in: .whitespaces).isEmpty,
                               let first = p.models.first {
                                draft.judgeModel = first
                            }
                        }
                    }
                } label: {
                    HStack(spacing: 6) {
                        Text(activeJudgeProvider?.name ?? "选择供应商")
                            .font(.system(size: 12.5))
                            .foregroundStyle(activeJudgeProvider == nil ? EjaTheme.inkMuted : EjaTheme.ink)
                            .lineLimit(1)
                        Spacer(minLength: 4)
                        Image(systemName: "chevron.up.chevron.down")
                            .font(.system(size: 9.5, weight: .medium))
                            .foregroundStyle(EjaTheme.inkMuted)
                    }
                    .frame(width: 128)
                    .ejaField()
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()

                HStack(spacing: 6) {
                    TextField(
                        "",
                        text: $draft.judgeModel,
                        prompt: Text("模型名，如 deepseek-chat / gpt-4o").foregroundStyle(EjaTheme.inkMuted)
                    )
                    .font(.system(size: 12.5))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .focused($focusedField, equals: "judge-model")

                    if let models = activeJudgeProvider?.models, !models.isEmpty {
                        Menu {
                            ForEach(models, id: \.self) { m in
                                Button(m) { draft.judgeModel = m }
                            }
                        } label: {
                            Image(systemName: "chevron.down")
                                .font(.system(size: 9.5, weight: .medium))
                                .foregroundStyle(EjaTheme.inkMuted)
                                .frame(width: 18, height: 18)
                                .contentShape(Rectangle())
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .fixedSize()
                    }
                }
                .ejaField(focused: focusedField == "judge-model")
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 12)

            hairlineHorizontal

            HStack(spacing: 0) {
                VStack(spacing: 0) {
                    HStack {
                        Text("供应商")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(EjaTheme.inkMuted)
                        Spacer()
                        Menu {
                            Button("OpenAI") { draft.addPreset(id: "openai", name: "OpenAI", kind: "openai", baseUrl: "https://api.openai.com/v1", models: ["gpt-4o", "gpt-4o-mini"]) }
                            Button("DeepSeek") { draft.addPreset(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", models: ["deepseek-chat", "deepseek-reasoner"]) }
                            Button("Anthropic") { draft.addPreset(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", models: ["claude-sonnet-4-5", "claude-haiku-4-5"]) }
                            Button("OpenRouter") { draft.addPreset(id: "openrouter", name: "OpenRouter", kind: "openai", baseUrl: "https://openrouter.ai/api/v1", models: ["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]) }
                            Button("Ollama (本地)") { draft.addPreset(id: "ollama", name: "Ollama", kind: "openai", baseUrl: "http://127.0.0.1:11434/v1", models: ["qwen2.5:14b"]) }
                            Divider()
                            Button("自定义 OpenAI 兼容") { draft.addPreset(id: "custom", name: "自定义", kind: "openai", baseUrl: "", models: []) }
                        } label: {
                            Image(systemName: "plus")
                                .font(.system(size: 11.5, weight: .medium))
                                .foregroundStyle(EjaTheme.inkSecondary)
                                .frame(width: 22, height: 22)
                                .contentShape(Rectangle())
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .fixedSize()
                        .help("添加供应商")
                    }
                    .padding(.horizontal, 14)
                    .padding(.top, 10)
                    .padding(.bottom, 6)

                    ScrollView {
                        LazyVStack(spacing: 2) {
                            ForEach(draft.providers) { p in
                                providerSidebarRow(p)
                            }
                        }
                        .padding(.horizontal, 8)
                        .padding(.bottom, 10)
                    }
                    .scrollIndicators(.never)
                }
                .frame(width: 178)

                hairlineVertical

                if let idx = draft.providers.firstIndex(where: { $0.id == draft.selectedProviderId }) {
                    providerEditor(index: idx)
                } else {
                    Text("选择或添加一个供应商")
                        .font(.system(size: 12.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
        }
    }

    private func providerSidebarRow(_ provider: ProviderConfigDTO) -> some View {
        let selected = draft.selectedProviderId == provider.id
        let isMain = draft.judgeProviderId == provider.id
        return SidebarRowButton(selected: selected) {
            draft.selectedProviderId = provider.id
        } content: {
            HStack(spacing: 8) {
                VStack(alignment: .leading, spacing: 1.5) {
                    Text(provider.name.isEmpty ? provider.id : provider.name)
                        .font(.system(size: 12.5, weight: selected ? .medium : .regular))
                        .foregroundStyle(EjaTheme.ink)
                        .lineLimit(1)
                    Text(provider.kind == "anthropic" ? "Anthropic" : "OpenAI")
                        .font(.system(size: 10.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                }
                Spacer(minLength: 4)
                if isMain {
                    Circle()
                        .fill(EjaTheme.ink)
                        .frame(width: 5, height: 5)
                }
            }
        }
    }

    private func providerEditor(index: Int) -> some View {
        let provider = draft.providers[index]
        let nameBinding = Binding<String>(
            get: { draft.providers.indices.contains(index) ? draft.providers[index].name : "" },
            set: { if draft.providers.indices.contains(index) { draft.providers[index].name = $0 } }
        )
        let urlBinding = Binding<String>(
            get: { draft.providers.indices.contains(index) ? draft.providers[index].baseUrl : "" },
            set: { if draft.providers.indices.contains(index) { draft.providers[index].baseUrl = $0 } }
        )
        let keyBinding = Binding<String>(
            get: { draft.providers.indices.contains(index) ? draft.providers[index].apiKey : "" },
            set: { if draft.providers.indices.contains(index) { draft.providers[index].apiKey = $0 } }
        )
        let modelsBinding = Binding<String>(
            get: { draft.providers.indices.contains(index) ? draft.providers[index].models.joined(separator: ", ") : "" },
            set: { raw in
                guard draft.providers.indices.contains(index) else { return }
                draft.providers[index].models = raw.split(separator: ",").map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            }
        )
        let isMain = draft.judgeProviderId == provider.id
        return ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                formRow("名称") {
                    TextField("", text: nameBinding, prompt: Text("如 DeepSeek / OpenAI").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12.5))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .focused($focusedField, equals: "p-name")
                        .ejaField(focused: focusedField == "p-name")
                }

                formRow("协议") {
                    HStack(spacing: 2) {
                        protocolSegment(label: "OpenAI 兼容", tag: "openai", index: index)
                        protocolSegment(label: "Anthropic", tag: "anthropic", index: index)
                    }
                    .padding(2)
                    .background(
                        RoundedRectangle(cornerRadius: 8, style: .continuous)
                            .fill(EjaTheme.surfaceSubtle)
                    )
                    .fixedSize()
                }

                formRow("Base URL") {
                    TextField("", text: urlBinding, prompt: Text("https://api.openai.com/v1").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .focused($focusedField, equals: "p-url")
                        .ejaField(focused: focusedField == "p-url")
                }

                formRow("API Key") {
                    SecureField(
                        "",
                        text: keyBinding,
                        prompt: Text(provider.hasApiKey ? "已保存密钥，留空继续沿用" : "输入 API Key").foregroundStyle(EjaTheme.inkMuted)
                    )
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundStyle(EjaTheme.ink)
                    .textFieldStyle(.plain)
                    .focused($focusedField, equals: "p-key")
                    .ejaField(focused: focusedField == "p-key")
                }

                formRow("常用模型") {
                    TextField("", text: modelsBinding, prompt: Text("逗号分隔，如 deepseek-chat, deepseek-reasoner").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .focused($focusedField, equals: "p-models")
                        .ejaField(focused: focusedField == "p-models")
                }

                hairlineHorizontal
                    .padding(.top, 4)

                HStack(spacing: 8) {
                    Button("测试连接") {
                        let testModel = provider.models.first ?? draft.judgeModel
                        client.testProvider(provider, model: testModel.isEmpty ? "gpt-4o" : testModel)
                    }
                    .buttonStyle(EjaSecondaryButtonStyle(bordered: true))
                    .disabled(provider.baseUrl.trimmingCharacters(in: .whitespaces).isEmpty)

                    if !isMain {
                        Button("设为主模型") {
                            draft.judgeProviderId = provider.id
                            if let first = provider.models.first { draft.judgeModel = first }
                        }
                        .buttonStyle(EjaSecondaryButtonStyle())
                    }

                    Spacer()

                    Button("删除") {
                        let removedId = provider.id
                        draft.providers.remove(at: index)
                        draft.selectedProviderId = draft.providers.first?.id
                        if draft.judgeProviderId == removedId {
                            draft.judgeProviderId = draft.providers.first?.id ?? ""
                        }
                    }
                    .buttonStyle(EjaSecondaryButtonStyle(destructive: true))
                }

                if let res = client.providerTestResults[provider.id] {
                    HStack(spacing: 6) {
                        Circle()
                            .fill(res.ok ? EjaTheme.positive : EjaTheme.danger)
                            .frame(width: 5.5, height: 5.5)
                        Text(res.message)
                            .font(.system(size: 11.5))
                            .foregroundStyle(res.ok ? EjaTheme.positive : EjaTheme.danger)
                            .textSelection(.enabled)
                    }
                    .transition(.opacity)
                }
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 16)
            .animation(.easeOut(duration: 0.16), value: client.providerTestResults[provider.id]?.message)
        }
        .scrollIndicators(.never)
    }

    private func protocolSegment(label: String, tag: String, index: Int) -> some View {
        let active = draft.providers.indices.contains(index) && draft.providers[index].kind == tag
        return Button {
            guard draft.providers.indices.contains(index) else { return }
            withAnimation(.easeOut(duration: 0.18)) {
                draft.providers[index].kind = tag
            }
        } label: {
            Text(label)
                .font(.system(size: 12, weight: active ? .medium : .regular))
                .foregroundStyle(active ? EjaTheme.ink : EjaTheme.inkSecondary)
                .padding(.horizontal, 10)
                .padding(.vertical, 4.5)
                .background {
                    if active {
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(EjaTheme.surface)
                            .shadow(color: EjaTheme.subtleShadow, radius: 2, x: 0, y: 1)
                            .matchedGeometryEffect(id: "protocol-thumb", in: protocolNamespace)
                    }
                }
        }
        .buttonStyle(.plain)
    }

    private var connectorsTab: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                HStack(spacing: 4) {
                    Text("MCP")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(EjaTheme.inkMuted)
                    Spacer()
                    Button { client.reloadConnectors() } label: {
                        Image(systemName: "arrow.clockwise")
                            .font(.system(size: 10.5, weight: .medium))
                    }
                    .buttonStyle(EjaIconButtonStyle(size: 22))
                    .help("重载连接器")

                    Menu {
                        Button("Stdio 进程") { draft.addConnector(type: "stdio") }
                        Button("HTTP 服务") { draft.addConnector(type: "http") }
                    } label: {
                        Image(systemName: "plus")
                            .font(.system(size: 11.5, weight: .medium))
                            .foregroundStyle(EjaTheme.inkSecondary)
                            .frame(width: 22, height: 22)
                            .contentShape(Rectangle())
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .help("添加连接器")
                }
                .padding(.horizontal, 14)
                .padding(.top, 10)
                .padding(.bottom, 6)

                ScrollView {
                    LazyVStack(spacing: 2) {
                        ForEach(Array(draft.connectors.enumerated()), id: \.element.id) { idx, conn in
                            connectorSidebarRow(conn, index: idx)
                        }
                    }
                    .padding(.horizontal, 8)
                    .padding(.bottom, 10)
                }
                .scrollIndicators(.never)
            }
            .frame(width: 196)

            hairlineVertical

            if let idx = draft.connectors.firstIndex(where: { $0.id == draft.selectedConnectorId }) {
                connectorEditor(index: idx)
            } else {
                Text("暂无连接器，点击左侧 + 添加或由 EJA 写入 connectors.json")
                    .font(.system(size: 12.5))
                    .foregroundStyle(EjaTheme.inkMuted)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 28)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }

    private func connectorSidebarRow(_ conn: ConnectorConfigDTO, index: Int) -> some View {
        let selected = draft.selectedConnectorId == conn.id
        let st = client.connectorStatuses.first(where: { $0.id == conn.id })
        let dotColor: Color = !conn.enabled ? EjaTheme.inkMuted : (st?.connected == true ? EjaTheme.positive : EjaTheme.warning)
        let enabledBinding = Binding<Bool>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].enabled : false },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].enabled = $0 } }
        )
        return SidebarRowButton(selected: selected) {
            draft.selectedConnectorId = conn.id
        } content: {
            HStack(spacing: 8) {
                Circle()
                    .fill(dotColor)
                    .frame(width: 6, height: 6)
                VStack(alignment: .leading, spacing: 1.5) {
                    Text(conn.name.isEmpty ? conn.id : conn.name)
                        .font(.system(size: 12.5, weight: selected ? .medium : .regular))
                        .foregroundStyle(EjaTheme.ink)
                        .lineLimit(1)
                    Text("\(conn.transport.type) · \(st?.tools.count ?? 0) 工具")
                        .font(.system(size: 10.5))
                        .foregroundStyle(EjaTheme.inkMuted)
                }
                Spacer(minLength: 4)
                Toggle("", isOn: enabledBinding)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.mini)
            }
        }
    }

    private func connectorEditor(index: Int) -> some View {
        let conn = draft.connectors[index]
        let status = client.connectorStatuses.first(where: { $0.id == conn.id })
        let idBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].id : "" },
            set: { newVal in
                guard draft.connectors.indices.contains(index) else { return }
                let oldId = draft.connectors[index].id
                draft.connectors[index].id = newVal
                if draft.selectedConnectorId == oldId { draft.selectedConnectorId = newVal }
            }
        )
        let nameBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? draft.connectors[index].name : "" },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].name = $0 } }
        )
        let cmdBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? (draft.connectors[index].transport.command ?? "") : "" },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].transport.command = $0 } }
        )
        let argsBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? (draft.connectors[index].transport.args ?? []).joined(separator: " ") : "" },
            set: { raw in
                guard draft.connectors.indices.contains(index) else { return }
                draft.connectors[index].transport.args = raw.split(separator: " ").map(String.init)
            }
        )
        let urlBinding = Binding<String>(
            get: { draft.connectors.indices.contains(index) ? (draft.connectors[index].transport.url ?? "") : "" },
            set: { if draft.connectors.indices.contains(index) { draft.connectors[index].transport.url = $0 } }
        )
        let kvBinding = Binding<String>(
            get: {
                guard draft.connectors.indices.contains(index) else { return "" }
                let map = conn.transport.type == "http" ? (draft.connectors[index].transport.headers ?? [:]) : (draft.connectors[index].transport.env ?? [:])
                return map.keys.sorted().map { "\($0)=\(map[$0] ?? "")" }.joined(separator: "\n")
            },
            set: { raw in
                guard draft.connectors.indices.contains(index) else { return }
                var map: [String: String] = [:]
                for line in raw.split(separator: "\n") {
                    if let sep = line.firstIndex(of: "=") {
                        let k = String(line[..<sep]).trimmingCharacters(in: .whitespaces)
                        let v = String(line[line.index(after: sep)...]).trimmingCharacters(in: .whitespaces)
                        if !k.isEmpty { map[k] = v }
                    }
                }
                if draft.connectors[index].transport.type == "http" {
                    draft.connectors[index].transport.headers = map
                } else {
                    draft.connectors[index].transport.env = map
                }
            }
        )
        return ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                formRow("ID") {
                    TextField("", text: idBinding, prompt: Text("mcp-1").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .focused($focusedField, equals: "c-id")
                        .ejaField(focused: focusedField == "c-id")
                }

                formRow("名称") {
                    TextField("", text: nameBinding, prompt: Text("连接器名称").foregroundStyle(EjaTheme.inkMuted))
                        .font(.system(size: 12.5))
                        .foregroundStyle(EjaTheme.ink)
                        .textFieldStyle(.plain)
                        .focused($focusedField, equals: "c-name")
                        .ejaField(focused: focusedField == "c-name")
                }

                if conn.transport.type == "http" {
                    formRow("URL") {
                        TextField("", text: urlBinding, prompt: Text("http://127.0.0.1:8080/mcp").foregroundStyle(EjaTheme.inkMuted))
                            .font(.system(size: 12, design: .monospaced))
                            .foregroundStyle(EjaTheme.ink)
                            .textFieldStyle(.plain)
                            .focused($focusedField, equals: "c-url")
                            .ejaField(focused: focusedField == "c-url")
                    }

                    formRow("请求头", alignment: .top) {
                        TextField("", text: kvBinding, prompt: Text("KEY=VAL，每行一条").foregroundStyle(EjaTheme.inkMuted), axis: .vertical)
                            .font(.system(size: 11.5, design: .monospaced))
                            .foregroundStyle(EjaTheme.ink)
                            .textFieldStyle(.plain)
                            .lineLimit(2...4)
                            .focused($focusedField, equals: "c-kv")
                            .ejaField(focused: focusedField == "c-kv")
                    }
                } else {
                    formRow("命令") {
                        TextField("", text: cmdBinding, prompt: Text("npx / python3").foregroundStyle(EjaTheme.inkMuted))
                            .font(.system(size: 12, design: .monospaced))
                            .foregroundStyle(EjaTheme.ink)
                            .textFieldStyle(.plain)
                            .focused($focusedField, equals: "c-cmd")
                            .ejaField(focused: focusedField == "c-cmd")
                    }

                    formRow("参数") {
                        TextField("", text: argsBinding, prompt: Text("空格分隔").foregroundStyle(EjaTheme.inkMuted))
                            .font(.system(size: 12, design: .monospaced))
                            .foregroundStyle(EjaTheme.ink)
                            .textFieldStyle(.plain)
                            .focused($focusedField, equals: "c-args")
                            .ejaField(focused: focusedField == "c-args")
                    }

                    formRow("环境变量", alignment: .top) {
                        TextField("", text: kvBinding, prompt: Text("KEY=VAL，每行一条").foregroundStyle(EjaTheme.inkMuted), axis: .vertical)
                            .font(.system(size: 11.5, design: .monospaced))
                            .foregroundStyle(EjaTheme.ink)
                            .textFieldStyle(.plain)
                            .lineLimit(2...4)
                            .focused($focusedField, equals: "c-kv")
                            .ejaField(focused: focusedField == "c-kv")
                    }
                }

                if let err = status?.error {
                    Text(err)
                        .font(.system(size: 11.5))
                        .foregroundStyle(EjaTheme.danger)
                        .textSelection(.enabled)
                }

                if let tools = status?.tools, !tools.isEmpty {
                    hairlineHorizontal
                        .padding(.top, 2)
                    VStack(alignment: .leading, spacing: 8) {
                        Text("已发现工具 (\(tools.count))")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(EjaTheme.inkMuted)
                        ForEach(tools) { tool in
                            VStack(alignment: .leading, spacing: 2) {
                                Text(tool.capability)
                                    .font(.system(size: 11.5, design: .monospaced))
                                    .foregroundStyle(EjaTheme.ink)
                                if !tool.description.isEmpty {
                                    Text(tool.description)
                                        .font(.system(size: 11))
                                        .foregroundStyle(EjaTheme.inkSecondary)
                                        .lineLimit(2)
                                }
                            }
                        }
                    }
                }

                hairlineHorizontal
                    .padding(.top, 4)

                HStack {
                    Spacer()
                    Button("删除连接器") {
                        draft.connectors.remove(at: index)
                        draft.selectedConnectorId = draft.connectors.first?.id
                    }
                    .buttonStyle(EjaSecondaryButtonStyle(destructive: true))
                }
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 16)
        }
        .scrollIndicators(.never)
    }

    private func formRow<Content: View>(
        _ label: String,
        alignment: VerticalAlignment = .center,
        @ViewBuilder content: () -> Content
    ) -> some View {
        HStack(alignment: alignment, spacing: 12) {
            Text(label)
                .font(.system(size: 12))
                .foregroundStyle(EjaTheme.inkSecondary)
                .frame(width: 62, alignment: .leading)
                .padding(.top, alignment == .top ? 6 : 0)
            content()
        }
    }

    private var hairlineHorizontal: some View {
        Rectangle()
            .fill(EjaTheme.hairline)
            .frame(height: 1)
    }

    private var hairlineVertical: some View {
        Rectangle()
            .fill(EjaTheme.hairline)
            .frame(width: 1)
    }
}

private struct SidebarRowButton<Content: View>: View {
    let selected: Bool
    let action: () -> Void
    @ViewBuilder let content: () -> Content
    @StateObject private var hover = HoverState()

    var body: some View {
        Button(action: action) {
            content()
                .padding(.horizontal, 10)
                .padding(.vertical, 7)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(selected ? EjaTheme.surfaceSubtle : (hover.hovered ? EjaTheme.surfaceSubtle.opacity(0.55) : .clear))
                )
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .animation(.easeOut(duration: 0.14), value: selected)
        .animation(.easeOut(duration: 0.14), value: hover.hovered)
        .onHover { hover.hovered = $0 }
    }
}
