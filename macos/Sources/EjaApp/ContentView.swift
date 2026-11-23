import EjaClient
import SwiftUI

@MainActor
final class ContentUIState: ObservableObject {
    @Published var draftText: String = ""
    @Published var showSettingsSheet: Bool = false
    @Published var baselineId: Int64 = -1
    @Published var sendHovered: Bool = false

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
                ZStack(alignment: .top) {
                    ScrollViewReader { proxy in
                        ScrollView {
                            LazyVStack(alignment: .leading, spacing: 18) {
                                ForEach(visibleItems) { item in
                                    streamRow(item)
                                        .id(item.id)
                                        .transition(.opacity.combined(with: .offset(y: 4)))
                                }
                                if client.status != "idle" {
                                    ProgressView()
                                        .controlSize(.small)
                                        .padding(.leading, 4)
                                        .id("eja-busy")
                                }
                            }
                            .frame(maxWidth: 640, alignment: .leading)
                            .frame(maxWidth: .infinity)
                            .padding(.horizontal, 28)
                            .padding(.top, 16)
                            .padding(.bottom, 24)
                            .animation(.easeOut(duration: 0.18), value: visibleItems.count)
                        }
                        .contentMargins(.top, 38, for: .scrollContent)
                        .scrollIndicators(.never)
                        .mask(
                            LinearGradient(
                                stops: [
                                    .init(color: .clear, location: 0.0),
                                    .init(color: .black, location: 0.09),
                                    .init(color: .black, location: 1.0),
                                ],
                                startPoint: .top,
                                endPoint: .bottom
                            )
                        )
                        .onChange(of: visibleItems.last?.id) { _, lastId in
                            if let lastId {
                                withAnimation(.easeOut(duration: 0.18)) { proxy.scrollTo(lastId, anchor: .bottom) }
                            }
                        }
                        .onChange(of: client.status) { _, newStatus in
                            if newStatus != "idle" {
                                withAnimation(.easeOut(duration: 0.18)) { proxy.scrollTo("eja-busy", anchor: .bottom) }
                            }
                        }
                    }

                    topFade
                }

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

                    composer
                }
                .frame(maxWidth: 640)
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
        }
        .tint(EjaTheme.ink)
        .sheet(isPresented: $ui.showSettingsSheet) {
            SettingsView(client: client)
        }
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

    private var topFade: some View {
        LinearGradient(
            stops: [
                .init(color: EjaTheme.canvas, location: 0.0),
                .init(color: EjaTheme.canvas.opacity(0.92), location: 0.48),
                .init(color: EjaTheme.canvas.opacity(0.0), location: 1.0),
            ],
            startPoint: .top,
            endPoint: .bottom
        )
        .frame(height: 52)
        .frame(maxWidth: .infinity, alignment: .top)
        .ignoresSafeArea(edges: .top)
        .allowsHitTesting(false)
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

            Button { sendDraft() } label: {
                Image(systemName: "arrow.up")
                    .font(.system(size: 11.5, weight: .semibold))
                    .foregroundStyle(canSend ? EjaTheme.canvas : EjaTheme.inkMuted)
                    .frame(width: 26, height: 26)
                    .background(
                        Circle()
                            .fill(canSend ? EjaTheme.ink.opacity(ui.sendHovered ? 0.86 : 1.0) : EjaTheme.surfaceSubtle)
                    )
            }
            .buttonStyle(SendScaleButtonStyle())
            .disabled(!canSend)
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
                Spacer(minLength: 64)
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
        default:
            HStack {
                markdownText(item.text)
                    .font(.system(size: 13.5))
                    .lineSpacing(4.5)
                    .foregroundStyle(EjaTheme.ink)
                    .textSelection(.enabled)
                    .padding(.leading, 2)
                Spacer(minLength: 36)
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

private struct SendScaleButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.92 : 1.0)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}
