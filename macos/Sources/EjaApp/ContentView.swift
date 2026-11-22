import EjaClient
import SwiftUI

@MainActor
final class ContentUIState: ObservableObject {
    @Published var draftText: String = ""
    @Published var showSettingsSheet: Bool = false
    @Published var showSubjectState: Bool = false
    @Published var expandedEvents: Set<Int64> = []

    func toggleEvent(_ id: Int64) {
        if expandedEvents.contains(id) {
            expandedEvents.remove(id)
        } else {
            expandedEvents.insert(id)
        }
    }
}

struct ContentView: View {
    @ObservedObject var client: HostClient
    @StateObject private var ui = ContentUIState()
    @FocusState private var composerFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            if client.isReady && !client.isConfigured {
                HStack(spacing: 10) {
                    Image(systemName: "key.fill").foregroundStyle(.orange)
                    Text("请先在设置中配置 LLM Provider 与主模型。").font(.callout)
                    Spacer()
                    Button("打开设置") { ui.showSettingsSheet = true }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .background(.orange.opacity(0.08))
            }

            if let err = client.errorBanner {
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.red)
                    Text(err).font(.callout).textSelection(.enabled)
                    Spacer()
                    Button { client.errorBanner = nil } label: { Image(systemName: "xmark").font(.caption) }
                        .buttonStyle(.plain)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
                .background(.red.opacity(0.08))
            }

            ScrollViewReader { proxy in
                ScrollView {
                    if client.stream.isEmpty {
                        VStack(spacing: 8) {
                            Text("EJA").font(.system(size: 26, weight: .semibold, design: .rounded))
                            Text("连续记忆 · 标准 MCP 连接器 · 可自进化原点")
                                .font(.callout)
                                .foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity)
                        .padding(.top, 120)
                    } else {
                        LazyVStack(alignment: .leading, spacing: 12) {
                            ForEach(client.stream) { item in
                                streamRow(item).id(item.id)
                            }
                            if client.status != "idle" {
                                HStack(spacing: 8) {
                                    ProgressView().controlSize(.small)
                                    Text(client.statusDetail).font(.caption).foregroundStyle(.secondary)
                                }
                                .padding(.horizontal, 10)
                                .id("eja-busy")
                            }
                        }
                        .padding(.horizontal, 20)
                        .padding(.vertical, 16)
                    }
                }
                .onChange(of: client.stream.last?.id) { _, lastId in
                    if let lastId {
                        withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo(lastId, anchor: .bottom) }
                    }
                }
                .onChange(of: client.status) { _, newStatus in
                    if newStatus != "idle" {
                        withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("eja-busy", anchor: .bottom) }
                    }
                }
            }

            Divider()

            HStack(alignment: .bottom, spacing: 10) {
                TextField("对 EJA 说点什么…", text: $ui.draftText, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(1...6)
                    .focused($composerFocused)
                    .onSubmit { sendDraft() }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 9)
                    .background(Color(nsColor: .textBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(Color.primary.opacity(0.1), lineWidth: 1))

                Button { sendDraft() } label: {
                    Image(systemName: "arrow.up.circle.fill").font(.system(size: 24))
                }
                .buttonStyle(.plain)
                .foregroundStyle(canSend ? Color.accentColor : Color.secondary.opacity(0.4))
                .disabled(!canSend)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .toolbar {
            ToolbarItem(placement: .navigation) {
                HStack(spacing: 6) {
                    Circle()
                        .fill(client.status == "thinking" ? .blue : (client.isConfigured ? .green : .secondary))
                        .frame(width: 8, height: 8)
                    Text(client.statusDetail).font(.subheadline).foregroundStyle(.secondary)
                }
            }
            ToolbarItemGroup(placement: .primaryAction) {
                Button { ui.showSubjectState.toggle() } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "brain")
                        let count = client.concerns.count + client.wakes.count
                        if count > 0 { Text("\(count)").font(.caption2.monospacedDigit()) }
                    }
                }
                .help("挂念与定时唤醒")
                .popover(isPresented: $ui.showSubjectState, arrowEdge: .bottom) {
                    SubjectStatePopoverView(client: client)
                }

                Button { ui.showSettingsSheet = true } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "slider.horizontal.3")
                        if client.connectedToolCount > 0 {
                            Text("\(client.connectedToolCount)").font(.caption2.monospacedDigit())
                        }
                    }
                }
                .help("LLM 与 MCP 连接器设置")
            }
        }
        .sheet(isPresented: $ui.showSettingsSheet) {
            SettingsView(client: client)
        }
        .onAppear { composerFocused = true }
    }

    private var canSend: Bool {
        !ui.draftText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func sendDraft() {
        let text = ui.draftText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        ui.draftText = ""
        client.sendMessage(text)
    }

    @ViewBuilder
    private func streamRow(_ item: StreamItemDTO) -> some View {
        switch item.kind {
        case "person":
            HStack {
                Spacer(minLength: 64)
                Text(item.text)
                    .textSelection(.enabled)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .background(Color.accentColor.opacity(0.14))
                    .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
            }
        case "eja":
            HStack {
                VStack(alignment: .leading, spacing: 6) {
                    if item.proactive == true {
                        Label("主动唤醒", systemImage: "sparkles")
                            .font(.caption2.weight(.medium))
                            .foregroundStyle(Color.accentColor)
                    }
                    markdownText(item.text)
                        .textSelection(.enabled)
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 11)
                .background(Color(nsColor: .controlBackgroundColor))
                .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).stroke(Color.primary.opacity(0.06), lineWidth: 1))
                Spacer(minLength: 48)
            }
        default:
            let expanded = ui.expandedEvents.contains(item.id)
            let hasDetail = !(item.detail?.isEmpty ?? true)
            VStack(alignment: .leading, spacing: 6) {
                Button {
                    if hasDetail { ui.toggleEvent(item.id) }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "terminal").font(.caption2).foregroundStyle(.secondary)
                        Text(item.text).font(.caption).foregroundStyle(.secondary)
                        if hasDetail {
                            Image(systemName: expanded ? "chevron.up" : "chevron.down").font(.caption2).foregroundStyle(.tertiary)
                        }
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 5)
                    .background(Color.primary.opacity(0.04))
                    .clipShape(Capsule())
                }
                .buttonStyle(.plain)

                if expanded, let detail = item.detail {
                    Text(detail)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color(nsColor: .textBackgroundColor))
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                }
            }
        }
    }

    private func markdownText(_ source: String) -> Text {
        if let attributed = try? AttributedString(
            markdown: source,
            options: AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        ) {
            return Text(attributed)
        }
        return Text(source)
    }
}

private struct SubjectStatePopoverView: View {
    @ObservedObject var client: HostClient

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("主体状态").font(.headline)
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    GroupBox("挂念 (\(client.concerns.count))") {
                        if client.concerns.isEmpty {
                            Text("暂无").font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                        } else {
                            VStack(alignment: .leading, spacing: 4) {
                                ForEach(client.concerns) { Text("• \($0.text)").font(.callout).textSelection(.enabled) }
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    GroupBox("定时唤醒 (\(client.wakes.count))") {
                        if client.wakes.isEmpty {
                            Text("暂无").font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                        } else {
                            VStack(alignment: .leading, spacing: 4) {
                                ForEach(client.wakes) { Text("\($0.fireAtDisplay) · \($0.reason)").font(.callout) }
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                }
            }
        }
        .padding(14)
        .frame(width: 320, height: 340)
    }
}
