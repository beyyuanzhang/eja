import EjaClient
import SwiftUI

@MainActor
final class ContentUIState: ObservableObject {
    @Published var draftText: String = ""
    @Published var showSettingsSheet: Bool = false
    @Published var baselineId: Int64 = -1
    @Published var sendHovered: Bool = false
    @Published var expandedEvents: Set<Int64> = []
    @Published var showAgenda: Bool = false

    func captureBaselineIfNeeded(isReady: Bool, dataDir: String, from stream: [StreamItemDTO]) {
        guard isReady, baselineId < 0 else { return }
        if !dataDir.isEmpty {
            let file = UiRuntime.reloadBaselineFile(in: URL(fileURLWithPath: dataDir, isDirectory: true))
            if let raw = try? String(contentsOf: file, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines),
               let saved = Int64(raw) {
                try? FileManager.default.removeItem(at: file)
                baselineId = saved
                return
            }
        }
        baselineId = stream.last?.id ?? 0
    }
}

struct ContentView: View {
    @ObservedObject var client: HostClient
    @ObservedObject var ui: ContentUIState
    @FocusState private var composerFocused: Bool

    var body: some View {
        ZStack(alignment: .topTrailing) {
            EjaTheme.canvas.ignoresSafeArea()

            VStack(spacing: 0) {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 18) {
                            if canLoadEarlier {
                                Button("加载更早", action: loadEarlier)
                                    .buttonStyle(.plain)
                                    .font(.system(size: 12))
                                    .foregroundStyle(EjaTheme.inkMuted)
                                    .frame(maxWidth: .infinity)
                            }
                            ForEach(visibleItems) { item in
                                streamRow(item)
                                    .id(item.id)
                            }
                            if client.status != "idle" {
                                busyRow
                            }
                            Color.clear
                                .frame(height: 42)
                                .accessibilityHidden(true)
                                .id("eja-conversation-bottom")
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 28)
                    }
                    .contentMargins(.top, 72, for: .scrollContent)
                    .scrollIndicators(.never)
                    .clipped()
                    .mask {
                        GeometryReader { geometry in
                            let fadeHeight = min(60, geometry.size.height / 2)
                            VStack(spacing: 0) {
                                LinearGradient(stops: Self.edgeFadeStops, startPoint: .bottom, endPoint: .top)
                                    .frame(height: fadeHeight)
                                Color.black
                                LinearGradient(stops: Self.edgeFadeStops, startPoint: .top, endPoint: .bottom)
                                    .frame(height: fadeHeight)
                            }
                        }
                    }
                    .onChange(of: visibleItems.last?.id) {
                        proxy.scrollTo("eja-conversation-bottom", anchor: .bottom)
                    }
                    .onChange(of: client.status) { _, status in
                        if status != "idle" {
                            proxy.scrollTo("eja-conversation-bottom", anchor: .bottom)
                        }
                    }
                    .onChange(of: client.streamingText) { _, text in
                        if !text.isEmpty {
                            proxy.scrollTo("eja-conversation-bottom", anchor: .bottom)
                        }
                    }
                }
                .ignoresSafeArea(.container, edges: .top)

                VStack(spacing: 8) {
                    if let err = client.errorBanner {
                        HStack(spacing: 8) {
                            Text(err)
                                .font(.system(size: 12))
                                .foregroundStyle(EjaTheme.danger)
                                .textSelection(.enabled)
                            Spacer(minLength: 8)
                            Button { client.errorBanner = nil } label: {
                                Image(systemName: "xmark")
                                    .font(.system(size: 10, weight: .medium))
                            }
                            .buttonStyle(EjaIconButtonStyle(size: 20))
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                        .background(
                            RoundedRectangle(cornerRadius: 9, style: .continuous)
                                .fill(EjaTheme.dangerTint)
                        )
                        .transition(.opacity.combined(with: .offset(y: 4)))
                    }

                    if !client.concerns.isEmpty || !client.wakes.isEmpty {
                        agenda
                    }

                    composer
                }
                .frame(maxWidth: .infinity)
                .padding(.horizontal, 24)
                .padding(.top, 4)
                .padding(.bottom, 18)
                .animation(.easeOut(duration: 0.16), value: client.errorBanner != nil)
            }

            Button(action: showSettings) {
                Image(systemName: "slider.horizontal.3")
                    .font(.system(size: 13, weight: .regular))
            }
            .buttonStyle(EjaIconButtonStyle(size: 28))
            .padding(.top, 10)
            .padding(.trailing, 14)
            .ignoresSafeArea(edges: .top)

            if ui.showSettingsSheet && client.isReady {
                SettingsView(client: client, isPresented: $ui.showSettingsSheet)
                    .transition(.opacity)
            }
        }
        .tint(EjaTheme.ink)
        .onAppear {
            composerFocused = true
            ui.captureBaselineIfNeeded(isReady: client.isReady, dataDir: client.dataDir, from: client.stream)
        }
        .onChange(of: client.isReady) { _, ready in
            ui.captureBaselineIfNeeded(isReady: ready, dataDir: client.dataDir, from: client.stream)
        }
    }

    private func showSettings() {
        ui.showSettingsSheet = true
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField("", text: $ui.draftText, axis: .vertical)
            .font(.system(size: 13.5))
            .foregroundStyle(EjaTheme.ink)
            .textFieldStyle(.plain)
            .lineLimit(1...6)
            .focused($composerFocused)
            .onSubmit { sendDraft() }
            .padding(.leading, 8)
            .padding(.vertical, 5)

            Button { canStop ? client.cancelTurn() : sendDraft() } label: {
                Image(systemName: canStop ? "stop.fill" : "arrow.up")
                    .font(.system(size: 11.5, weight: .semibold))
                    .foregroundStyle(canSend || canStop ? EjaTheme.canvas : EjaTheme.inkMuted)
                    .frame(width: 26, height: 26)
                    .background(
                        Circle()
                            .fill(canSend || canStop ? EjaTheme.ink.opacity(ui.sendHovered ? 0.86 : 1.0) : EjaTheme.surfaceSubtle)
                    )
            }
            .buttonStyle(SendScaleButtonStyle())
            .disabled(!canSend && !canStop)
            .onHover { ui.sendHovered = $0 }
            .animation(.easeOut(duration: 0.15), value: canSend)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 6)
        .background(
            RoundedRectangle(cornerRadius: 15, style: .continuous)
                .fill(EjaTheme.surface)
                .shadow(color: EjaTheme.subtleShadow, radius: 8, x: 0, y: 2)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 15, style: .continuous)
                .stroke(composerFocused ? EjaTheme.hairlineStrong : EjaTheme.hairline, lineWidth: 1)
        )
        .animation(.easeOut(duration: 0.16), value: composerFocused)
    }

    private var visibleItems: [StreamItemDTO] {
        guard ui.baselineId >= 0 else { return [] }
        return client.stream.filter { $0.id > ui.baselineId }
    }

    private var canLoadEarlier: Bool {
        ui.baselineId >= 0 && (client.stream.contains { $0.id <= ui.baselineId } || client.canLoadEarlier)
    }

    private func loadEarlier() {
        let hidden = client.stream.contains { $0.id <= ui.baselineId }
        ui.baselineId = 0
        if !hidden { client.loadHistory() }
    }

    private var agenda: some View {
        VStack(alignment: .leading, spacing: 5) {
            Button { ui.showAgenda.toggle() } label: {
                HStack(spacing: 4) {
                    Text(verbatim: "挂念 \(client.concerns.count) · 唤醒 \(client.wakes.count)")
                    Image(systemName: ui.showAgenda ? "chevron.down" : "chevron.right")
                        .font(.system(size: 9))
                }
            }
            .buttonStyle(.plain)
            if ui.showAgenda {
                ForEach(client.concerns) { concern in
                    Text(verbatim: "· \(concern.text)")
                        .lineLimit(2)
                        .textSelection(.enabled)
                }
                ForEach(client.wakes) { wake in
                    HStack(spacing: 6) {
                        Text(verbatim: "\(wake.fireAtDisplay) · \(wake.reason)")
                            .lineLimit(1)
                        Spacer(minLength: 8)
                        Button { client.dismissWake(id: wake.id) } label: {
                            Image(systemName: "xmark")
                                .font(.system(size: 9, weight: .medium))
                        }
                        .buttonStyle(EjaIconButtonStyle(size: 18))
                        .help("取消此唤醒")
                    }
                }
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(EjaTheme.inkSecondary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 8)
    }

    private var streamingPreview: String {
        client.streamingText.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    @ViewBuilder
    private var busyRow: some View {
        let preview = streamingPreview
        if !preview.isEmpty {
            ejaTextRow(preview)
        } else {
            HStack(spacing: 7) {
                ProgressView()
                    .controlSize(.small)
                if !client.statusDetail.isEmpty {
                    Text(client.statusDetail)
                        .font(.system(size: 12))
                        .foregroundStyle(EjaTheme.inkMuted)
                }
            }
            .padding(.leading, 4)
        }
    }

    private var canSend: Bool {
        client.isReady && !ui.draftText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private var canStop: Bool {
        client.status == "thinking" && ui.draftText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func sendDraft() {
        let text = ui.draftText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard client.isReady, !text.isEmpty else { return }
        if !client.isConfigured {
            ui.showSettingsSheet = true
            return
        }
        ui.draftText = ""
        client.sendMessage(text)
    }

    @ViewBuilder
    private func streamRow(_ item: StreamItemDTO) -> some View {
        switch item.kind {
        case "person":
            HStack {
                Spacer(minLength: 48)
                Text(item.text)
                    .font(.system(size: 13.5))
                    .lineSpacing(3)
                    .foregroundStyle(EjaTheme.ink)
                    .textSelection(.enabled)
                    .padding(.horizontal, 13)
                    .padding(.vertical, 8.5)
                    .background(EjaTheme.surfaceSubtle)
                    .clipShape(RoundedRectangle(cornerRadius: 13, style: .continuous))
            }
        case "event":
            eventRow(item)
        default:
            ejaTextRow(item.text)
        }
    }

    private func eventRow(_ item: StreamItemDTO) -> some View {
        let summary: String
        if let status = item.outcomeStatus {
            let label = switch status {
            case "observed": "已观测"
            case "failed": "失败"
            case "unknown": "未知"
            default: "等待回执"
            }
            let reference = item.receiptID.map { "回执 #\($0)" } ?? "工具"
            summary = "\(reference) · \(label) · \(item.text)"
        } else {
            summary = item.text
        }
        let expanded = ui.expandedEvents.contains(item.id)
        return VStack(alignment: .leading, spacing: 4) {
            if let detail = item.detail {
                Button {
                    if expanded { ui.expandedEvents.remove(item.id) } else { ui.expandedEvents.insert(item.id) }
                } label: {
                    HStack(spacing: 4) {
                        Text(verbatim: summary)
                        Image(systemName: expanded ? "chevron.down" : "chevron.right")
                            .font(.system(size: 9))
                    }
                }
                .buttonStyle(.plain)
                if expanded {
                    Text(verbatim: detail)
                        .font(.system(size: 11.5, design: .monospaced))
                        .foregroundStyle(EjaTheme.inkMuted)
                        .textSelection(.enabled)
                }
            } else {
                Text(verbatim: summary)
                    .textSelection(.enabled)
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(EjaTheme.inkSecondary)
    }

    private func ejaTextRow(_ text: String) -> some View {
        markdownText(text)
            .font(.system(size: 13.5))
            .lineSpacing(4.5)
            .foregroundStyle(EjaTheme.ink)
            .textSelection(.enabled)
            .padding(.leading, 2)
            .frame(maxWidth: .infinity, alignment: .leading)
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

    private static let edgeFadeStops: [Gradient.Stop] = (0...16).map { step in
        let position = Double(step) / 16
        let opacity = 1 - position * position * (3 - 2 * position)
        return .init(color: .black.opacity(opacity), location: CGFloat(position))
    }
}

private struct SendScaleButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.92 : 1.0)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}
