import ejaClient
import SwiftUI

@MainActor
final class ContentUIState: ObservableObject {
    @Published var draftText: String = ""
    @Published var settingsOpen: Bool = false
    @Published var baselineId: Int64 = -1
    @Published var sendHovered: Bool = false
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
            ejaTheme.canvas.ignoresSafeArea()

            VStack(spacing: 0) {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 18) {
                            edgeSpace
                            if canLoadEarlier {
                                Button(action: loadEarlier) {
                                    Image(systemName: "chevron.up")
                                }
                                .buttonStyle(.plain)
                                .font(.callout)
                                .foregroundStyle(ejaTheme.inkMuted)
                                .frame(maxWidth: .infinity)
                            }
                            ForEach(visibleItems) { item in
                                streamRow(item)
                                    .id(item.id)
                            }
                            if client.status != "idle" {
                                busyRow
                            }
                            edgeSpace
                                .accessibilityHidden(true)
                                .id("eja-conversation-bottom")
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 28)
                    }
                    .scrollIndicators(.never)
                    .clipped()
                    .mask {
                        GeometryReader { geometry in
                            let fadeHeight = geometry.size.height * 0.1
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
                        Button { client.errorBanner = nil } label: {
                            Image(systemName: "exclamationmark.circle.fill")
                                .font(.title3)
                                .foregroundStyle(ejaTheme.danger)
                                .symbolEffect(.bounce, value: err)
                        }
                        .buttonStyle(.plain)
                        .help(err)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 8)
                        .transition(.opacity)
                    }

                    if !client.wakes.isEmpty {
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

            if ui.settingsOpen {
                Color.black.opacity(0.12)
                    .ignoresSafeArea()
                    .onTapGesture {
                        withAnimation(.smooth) { ui.settingsOpen = false }
                    }
                    .transition(.opacity)

                GeometryReader { window in
                    let portrait = window.size.width < window.size.height
                    HStack(spacing: 0) {
                        Divider()
                            .ignoresSafeArea(edges: .top)
                        SettingsView(client: client)
                    }
                    .frame(width: window.size.width * (portrait ? 1 : 0.5))
                    .shadow(color: ejaTheme.subtleShadow, radius: 16, x: -4)
                    .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .transition(.move(edge: .trailing))
            }

            Button {
                withAnimation(.smooth) { ui.settingsOpen.toggle() }
            } label: {
                Image(systemName: ui.settingsOpen ? "xmark" : "slider.horizontal.3")
                    .font(.body)
                    .contentTransition(.symbolEffect(.replace))
                    .symbolEffect(.pulse, isActive: client.isReady && !client.isConfigured)
            }
            .buttonStyle(ejaIconButtonStyle())
            .accessibilityLabel("设置")
            .keyboardShortcut(ui.settingsOpen ? .cancelAction : nil)
            .disabled(!client.isReady)
            .padding(.top, 4)
            .padding(.trailing, 14)
        }
        .tint(ejaTheme.ink)
        .onChange(of: ui.settingsOpen) { _, open in
            composerFocused = !open
        }
        .onAppear {
            composerFocused = true
            ui.captureBaselineIfNeeded(isReady: client.isReady, dataDir: client.dataDir, from: client.stream)
        }
        .onChange(of: client.isReady) { _, ready in
            ui.captureBaselineIfNeeded(isReady: ready, dataDir: client.dataDir, from: client.stream)
        }
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField("", text: $ui.draftText, axis: .vertical)
            .font(.body)
            .foregroundStyle(ejaTheme.ink)
            .textFieldStyle(.plain)
            .lineLimit(1...6)
            .focused($composerFocused)
            .onSubmit { sendDraft() }
            .padding(.leading, 8)
            .padding(.vertical, 5)

            Button { canStop ? client.cancelTurn() : sendDraft() } label: {
                Image(systemName: canStop ? "stop.circle.fill" : "arrow.up.circle.fill")
                    .font(.title)
                    .symbolRenderingMode(.palette)
                    .foregroundStyle(
                        canSend || canStop ? ejaTheme.canvas : ejaTheme.inkMuted,
                        canSend || canStop ? ejaTheme.ink.opacity(ui.sendHovered ? 0.86 : 1.0) : ejaTheme.surfaceSubtle
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
                .fill(ejaTheme.surface)
                .shadow(color: ejaTheme.subtleShadow, radius: 8, x: 0, y: 2)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 15, style: .continuous)
                .stroke(composerFocused ? ejaTheme.hairlineStrong : ejaTheme.hairline, lineWidth: 1)
        )
        .animation(.easeOut(duration: 0.16), value: composerFocused)
    }

    private var visibleItems: [StreamItemDTO] {
        guard ui.baselineId >= 0 else { return [] }
        return client.stream.filter { $0.id > ui.baselineId && $0.kind != "event" }
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
                    Image(systemName: "alarm")
                    Text(verbatim: "\(client.wakes.count)")
                    Image(systemName: ui.showAgenda ? "chevron.down" : "chevron.right")
                        .font(.caption2)
                }
            }
            .buttonStyle(.plain)
            if ui.showAgenda {
                ForEach(client.wakes) { wake in
                    HStack(spacing: 6) {
                        Text(verbatim: "\(wake.fireAtDisplay) · \(wake.reason)")
                            .lineLimit(1)
                        Spacer(minLength: 8)
                        Button { client.dismissWake(id: wake.id) } label: {
                            Image(systemName: "xmark")
                                .font(.caption2.weight(.medium))
                        }
                        .buttonStyle(ejaIconButtonStyle())
                    }
                }
            }
        }
        .font(.callout)
        .foregroundStyle(ejaTheme.inkSecondary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 8)
    }

    private var edgeSpace: some View {
        Color.clear.containerRelativeFrame(.vertical) { height, _ in height * 0.1 }
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
            Image(systemName: "circle.fill")
                .font(.caption2)
                .imageScale(.small)
                .foregroundStyle(ejaTheme.inkMuted)
                .symbolEffect(.pulse)
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
            withAnimation(.smooth) { ui.settingsOpen = true }
            return
        }
        ui.draftText = ""
        client.sendMessage(text)
    }

    @ViewBuilder
    private func streamRow(_ item: StreamItemDTO) -> some View {
        if item.kind == "person" {
            HStack {
                Spacer(minLength: 48)
                Text(item.text)
                    .font(.body)
                    .lineSpacing(3)
                    .foregroundStyle(ejaTheme.ink)
                    .textSelection(.enabled)
                    .padding(.horizontal, 13)
                    .padding(.vertical, 8.5)
                    .background(ejaTheme.surfaceSubtle)
                    .clipShape(RoundedRectangle(cornerRadius: 13, style: .continuous))
            }
        } else {
            ejaTextRow(item.text)
        }
    }

    private func ejaTextRow(_ text: String) -> some View {
        markdownText(text)
            .font(.body)
            .lineSpacing(4.5)
            .foregroundStyle(ejaTheme.ink)
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
