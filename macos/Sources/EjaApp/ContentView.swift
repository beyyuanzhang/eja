import EjaClient
import SwiftUI

@MainActor
final class ContentUIState: ObservableObject {
    @Published var draftText: String = ""
    @Published var showSettingsSheet: Bool = false
    @Published var baselineId: Int64 = -1

    func captureBaselineIfNeeded(isReady: Bool, from stream: [StreamItemDTO]) {
        guard isReady, baselineId < 0 else { return }
        baselineId = stream.last?.id ?? 0
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

            ZStack(alignment: .top) {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 12) {
                            ForEach(visibleItems) { item in
                                streamRow(item).id(item.id)
                            }
                            if client.status != "idle" {
                                BusyDots()
                                    .padding(.horizontal, 14)
                                    .id("eja-busy")
                            }
                        }
                        .padding(.horizontal, 20)
                        .padding(.vertical, 16)
                    }
                    .contentMargins(.top, 34, for: .scrollContent)
                    .scrollIndicators(.never)
                    .mask(
                        LinearGradient(
                            stops: [
                                .init(color: .clear, location: 0.0),
                                .init(color: .black, location: 0.10),
                                .init(color: .black, location: 1.0),
                            ],
                            startPoint: .top,
                            endPoint: .bottom
                        )
                    )
                    .onChange(of: visibleItems.last?.id) { _, lastId in
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

                topFade
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
            ToolbarItemGroup(placement: .primaryAction) {
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
        .onAppear {
            composerFocused = true
            ui.captureBaselineIfNeeded(isReady: client.isReady, from: client.stream)
        }
        .onChange(of: client.isReady) { _, ready in
            ui.captureBaselineIfNeeded(isReady: ready, from: client.stream)
        }
    }

    private var topFade: some View {
        LinearGradient(
            stops: [
                .init(color: Color(nsColor: .windowBackgroundColor), location: 0.0),
                .init(color: Color(nsColor: .windowBackgroundColor).opacity(0.92), location: 0.45),
                .init(color: Color(nsColor: .windowBackgroundColor).opacity(0.0), location: 1.0),
            ],
            startPoint: .top,
            endPoint: .bottom
        )
        .frame(height: 52)
        .frame(maxWidth: .infinity, alignment: .top)
        .ignoresSafeArea(edges: .top)
        .allowsHitTesting(false)
    }

    private var visibleItems: [StreamItemDTO] {
        guard ui.baselineId >= 0 else { return [] }
        return client.stream.filter { item in
            item.id > ui.baselineId && (item.kind == "person" || item.kind == "eja")
        }
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
        default:
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

/// 等待指示：约一秒后仍未完成才浮现，三个点缓慢明灭。
@MainActor
private final class BusyDotsModel: ObservableObject {
    @Published var visible = false
    @Published var lit = false

    init() {
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 900_000_000)
            self.visible = true
            self.lit = true
        }
    }
}

private struct BusyDots: View {
    @StateObject private var model = BusyDotsModel()

    var body: some View {
        HStack(spacing: 5) {
            ForEach(0..<3, id: \.self) { i in
                Circle()
                    .fill(Color.secondary)
                    .frame(width: 4, height: 4)
                    .opacity(model.visible ? (model.lit ? 0.7 : 0.2) : 0)
                    .animation(
                        .easeInOut(duration: 0.6)
                        .repeatForever(autoreverses: true)
                        .delay(Double(i) * 0.14),
                        value: model.lit
                    )
            }
        }
        .frame(height: 16)
    }
}
