import SwiftUI

struct ReturnView: View {
    let model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    if let presence = model.presence {
                        MarkdownText(text: presence.text)
                            .font(Theme.serif(30, weight: .regular))
                            .lineSpacing(9)
                            .foregroundStyle(Theme.ink)
                            .textSelection(.enabled)
                            .frame(maxWidth: 720, alignment: .leading)
                            .padding(.top, 64)
                    }

                    if !model.loops.isEmpty {
                        VStack(spacing: 0) {
                            ForEach(model.loops) { loop in
                                LoopRow(loop: loop) { model.openLoop(loop) }
                            }
                        }
                        .padding(.top, model.presence == nil ? 64 : 50)
                    }

                    if !model.backgroundTasks.isEmpty {
                        BackgroundLine(task: model.backgroundTasks.first)
                            .padding(.top, 26)
                    }
                }
                .frame(maxWidth: 860, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.horizontal, 34)
                .padding(.bottom, 40)
            }

            ComposerView(model: model, placeholder: "说一件新的事…", showsBye: false)
        }
    }
}

struct LoopRow: View {
    let loop: LoopItem
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        VStack(spacing: 0) {
            Divider().overlay(Theme.line)
            Button(action: action) {
                HStack(spacing: 15) {
                    Circle()
                        .fill(Theme.amber)
                        .frame(width: 8, height: 8)
                    Text(loop.title)
                        .font(Theme.serif(19, weight: .medium))
                        .foregroundStyle(hovering ? Theme.ink : Theme.inkSoft)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Spacer(minLength: 8)
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(Theme.faint)
                        .opacity(hovering ? 1 : 0)
                }
                .padding(.vertical, 20)
                .padding(.horizontal, 5)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .onHover { hovering = $0 }
            Divider().overlay(Theme.line)
        }
    }
}

struct BackgroundLine: View {
    let task: TaskItem?

    var body: some View {
        HStack(spacing: 9) {
            BreathingDot(color: Theme.accent)
            if let task, !task.title.isEmpty {
                Text(task.title)
                    .font(.system(size: 11.5))
                    .foregroundStyle(Theme.muted)
                    .lineLimit(1)
            }
        }
    }
}

struct BreathingDot: View {
    let color: Color
    @State private var pulsing = false

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 7, height: 7)
            .opacity(pulsing ? 0.35 : 1)
            .animation(.easeInOut(duration: 1.1).repeatForever(autoreverses: true), value: pulsing)
            .onAppear { pulsing = true }
    }
}
