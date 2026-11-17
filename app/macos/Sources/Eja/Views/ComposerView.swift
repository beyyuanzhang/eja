import AppKit
import SwiftUI

struct ComposerView: View {
    let model: AppModel
    let placeholder: String
    let showsBye: Bool

    @State private var draft = ""
    @FocusState private var focused: Bool

    private var canSend: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !model.busy
    }

    var body: some View {
        HStack(alignment: .bottom, spacing: 10) {
            if showsBye {
                Button {
                    model.leaveEncounter()
                } label: {
                    Image(systemName: "rectangle.portrait.and.arrow.right")
                        .font(.system(size: 13, weight: .regular))
                        .foregroundStyle(Theme.muted)
                        .frame(width: 32, height: 32)
                }
                .buttonStyle(.plain)
                .disabled(model.busy)
                .help("bye · 结束这次会面")
                .padding(.bottom, 3)
            }

            GrowingEditor(text: $draft, placeholder: placeholder, onSubmit: submit)
                .focused($focused)
                .disabled(model.busy)

            Button(action: submit) {
                Image(systemName: "arrow.up")
                    .font(.system(size: 13, weight: .bold))
                    .foregroundStyle(canSend ? Theme.surface : Theme.faint)
                    .frame(width: 30, height: 30)
                    .background(canSend ? Theme.accent : Theme.surface2, in: Circle())
            }
            .buttonStyle(.plain)
            .disabled(!canSend)
            .help("发送（⏎）")
            .padding(.bottom, 3)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 20))
        .overlay(RoundedRectangle(cornerRadius: 20).strokeBorder(focused ? Theme.lineStrong : Theme.line))
        .shadow(color: Theme.ink.opacity(0.05), radius: 16, y: 6)
        .frame(maxWidth: 860)
        .padding(.leading, 74)
        .padding(.trailing, 34)
        .padding(.bottom, 18)
        .padding(.top, 6)
        .frame(maxWidth: .infinity)
        .onChange(of: model.draftSeed) {
            if !model.draftSeed.isEmpty {
                draft = model.draftSeed
                model.draftSeed = ""
                focused = true
            }
        }
        .onAppear { focused = true }
    }

    private func submit() {
        guard canSend else { return }
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        draft = ""
        Task { await model.send(text) }
    }
}

struct GrowingEditor: NSViewRepresentable {
    @Binding var text: String
    let placeholder: String
    let onSubmit: () -> Void

    func makeNSView(context: Context) -> NSScrollView {
        let textView = SubmitTextView()
        textView.delegate = context.coordinator
        textView.onSubmit = onSubmit
        textView.isRichText = false
        textView.allowsUndo = true
        textView.font = .systemFont(ofSize: 13.5)
        textView.textColor = NSColor(Theme.ink)
        textView.backgroundColor = .clear
        textView.drawsBackground = false
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.textContainerInset = NSSize(width: 0, height: 6)
        textView.textContainer?.widthTracksTextView = true
        textView.placeholder = placeholder

        let scrollView = NSScrollView()
        scrollView.documentView = textView
        scrollView.hasVerticalScroller = false
        scrollView.drawsBackground = false
        scrollView.autohidesScrollers = true
        scrollView.setContentHuggingPriority(.required, for: .vertical)
        scrollView.setContentCompressionResistancePriority(.required, for: .vertical)
        let height = scrollView.heightAnchor.constraint(equalToConstant: 32)
        height.isActive = true
        context.coordinator.textView = textView
        context.coordinator.heightConstraint = height
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        guard let textView = scrollView.documentView as? SubmitTextView else { return }
        textView.onSubmit = onSubmit
        textView.placeholder = placeholder
        textView.isEditable = context.environment.isEnabled
        if textView.string != text {
            textView.string = text
            textView.needsDisplay = true
        }
        context.coordinator.resize(textView)
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(text: $text)
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        let text: Binding<String>
        weak var textView: SubmitTextView?
        var heightConstraint: NSLayoutConstraint?
        private var height: CGFloat = 32

        init(text: Binding<String>) {
            self.text = text
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else { return }
            text.wrappedValue = textView.string
            resize(textView)
        }

        func resize(_ textView: NSTextView) {
            guard let container = textView.textContainer, let layout = textView.layoutManager else { return }
            layout.ensureLayout(for: container)
            let used = layout.usedRect(for: container).height + textView.textContainerInset.height * 2
            let next = min(132, max(32, ceil(used)))
            if abs(next - height) > 0.5 {
                height = next
                heightConstraint?.constant = next
                textView.invalidateIntrinsicContentSize()
            }
        }
    }

    final class SubmitTextView: NSTextView {
        var onSubmit: (() -> Void)?
        var placeholder = ""

        override var intrinsicContentSize: NSSize {
            guard let container = textContainer, let layout = layoutManager else {
                return NSSize(width: NSView.noIntrinsicMetric, height: 32)
            }
            layout.ensureLayout(for: container)
            let used = layout.usedRect(for: container).height + textContainerInset.height * 2
            return NSSize(width: NSView.noIntrinsicMetric, height: min(132, max(32, ceil(used))))
        }

        override func keyDown(with event: NSEvent) {
            let isReturn = event.keyCode == 36 || event.keyCode == 76
            if isReturn && !event.modifierFlags.contains(.shift) && !hasMarkedText() {
                onSubmit?()
                return
            }
            super.keyDown(with: event)
        }

        override func draw(_ dirtyRect: NSRect) {
            super.draw(dirtyRect)
            if string.isEmpty && !placeholder.isEmpty {
                let attributes: [NSAttributedString.Key: Any] = [
                    .font: font ?? NSFont.systemFont(ofSize: 13.5),
                    .foregroundColor: NSColor(Theme.faint),
                ]
                let origin = NSPoint(x: textContainerInset.width + 5, y: textContainerInset.height)
                placeholder.draw(at: origin, withAttributes: attributes)
            }
        }
    }
}
