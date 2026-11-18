import SwiftUI

struct EncounterView: View {
    let model: AppModel

    private var spoken: [ChatMessage] {
        model.encounterMessages.filter {
            !$0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !$0.choices.isEmpty
        }
    }

    private var visible: [ChatMessage] { Array(spoken.suffix(6)) }
    private var hiddenCount: Int { max(0, spoken.count - visible.count) }

    private var optimisticDuplicate: Bool {
        guard let optimistic = model.optimistic else { return false }
        let trimmed = optimistic.text.trimmingCharacters(in: .whitespacesAndNewlines)
        return model.encounterMessages.contains { message in
            message.role == "user"
                && message.text.trimmingCharacters(in: .whitespacesAndNewlines) == trimmed
                && message.at >= optimistic.at - 5
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    VStack(alignment: .leading, spacing: 22) {
                        if hiddenCount > 0 {
                            Text("…")
                                .font(.system(size: 15))
                                .foregroundStyle(Theme.faint)
                                .frame(maxWidth: .infinity)
                        }

                        ForEach(visible) { message in
                            VStack(alignment: .leading, spacing: 10) {
                                MessageBubble(message: message)
                                if message.id == spoken.last?.id, !message.choices.isEmpty {
                                    ChoiceButtons(question: message.id, choices: message.choices, model: model)
                                }
                            }
                            .id(message.id)
                        }

                        if let optimistic = model.optimistic, !optimisticDuplicate {
                            UserBubble(text: optimistic.text)
                                .opacity(0.55)
                        }

                        if model.busy {
                            if let lead = model.backgroundTasks.first {
                                BackgroundLine(task: lead)
                            } else {
                                ThinkingDots()
                            }
                        }

                        if !model.actions.isEmpty {
                            VStack(spacing: 12) {
                                ForEach(model.actions) { action in
                                    ActionCard(loop: action, model: model)
                                }
                            }
                            .padding(.top, 6)
                        }

                        Color.clear.frame(height: 1).id("bottom")
                    }
                    .frame(maxWidth: 860, alignment: .leading)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .padding(.horizontal, 34)
                    .padding(.top, 54)
                    .padding(.bottom, 26)
                }
                .scrollIndicators(.never)
                .onChange(of: visible.last?.id) {
                    withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("bottom", anchor: .bottom) }
                }
                .onChange(of: model.actions.count) {
                    withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("bottom", anchor: .bottom) }
                }
                .onChange(of: model.busy) {
                    withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("bottom", anchor: .bottom) }
                }
                .onAppear { proxy.scrollTo("bottom", anchor: .bottom) }
            }

            ComposerView(model: model, placeholder: "接着说…", showsBye: true)
        }
    }
}

struct MessageBubble: View {
    let message: ChatMessage

    var body: some View {
        if message.role == "user" {
            UserBubble(text: message.text)
        } else {
            MarkdownText(text: message.text)
                .font(.system(size: 14.5))
                .lineSpacing(5)
                .foregroundStyle(Theme.ink)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct UserBubble: View {
    let text: String

    var body: some View {
        HStack {
            Spacer(minLength: 60)
            Text(text)
                .font(.system(size: 13.5))
                .lineSpacing(4)
                .foregroundStyle(Theme.ink)
                .textSelection(.enabled)
                .padding(.horizontal, 15)
                .padding(.vertical, 10)
                .background(Theme.surface2, in: RoundedRectangle(cornerRadius: 16))
        }
    }
}

struct ThinkingDots: View {
    @State private var phase = false

    var body: some View {
        HStack(spacing: 5) {
            ForEach(0..<3, id: \.self) { index in
                Circle()
                    .fill(Theme.faint)
                    .frame(width: 5, height: 5)
                    .opacity(phase ? 1 : 0.3)
                    .animation(
                        .easeInOut(duration: 0.7).repeatForever(autoreverses: true).delay(Double(index) * 0.18),
                        value: phase
                    )
            }
        }
        .onAppear { phase = true }
        .padding(.vertical, 4)
    }
}

struct ActionCard: View {
    let loop: LoopItem
    let model: AppModel

    var body: some View {
        if loop.kind == "decision", let proposal = loop.proposal {
            decisionCard(proposal)
        } else if let task = loop.task {
            taskCard(task)
        }
    }

    private func decisionCard(_ proposal: Proposal) -> some View {
        HStack(alignment: .top, spacing: 14) {
            Text(proposal.text)
                .font(.system(size: 13))
                .lineSpacing(4)
                .foregroundStyle(Theme.inkSoft)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
            HStack(spacing: 8) {
                Button {
                    model.deny(proposal.id)
                } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.muted)
                        .frame(width: 30, height: 30)
                        .background(Theme.surface2, in: Circle())
                        .overlay(Circle().strokeBorder(Theme.line))
                }
                .buttonStyle(.plain)
                .disabled(model.busy)
                .help("拒绝")

                Button {
                    model.approve(proposal.id)
                } label: {
                    Image(systemName: "checkmark")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.surface)
                        .frame(width: 30, height: 30)
                        .background(Theme.accent, in: Circle())
                }
                .buttonStyle(.plain)
                .disabled(model.busy)
                .help("同意")
            }
        }
        .card()
    }

    private func taskCard(_ task: TaskItem) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Text(task.title)
                    .font(Theme.serif(16, weight: .medium))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(2)
                Spacer(minLength: 8)
                Button {
                    model.openTask(task.id)
                } label: {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(Theme.muted)
                        .frame(width: 28, height: 28)
                        .background(Theme.surface2, in: Circle())
                }
                .buttonStyle(.plain)
                .help("查看工作详情")
            }
            if let digest = task.run.digest, !digest.isEmpty {
                Text(clippedText(digest, limit: 220))
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.muted)
                    .lineSpacing(3)
            }
        }
        .card()
    }
}
