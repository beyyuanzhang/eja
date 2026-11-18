import SwiftUI

struct ChoiceButtons: View {
    let question: Int
    let choices: [ChoiceView]
    let model: AppModel

    var body: some View {
        LazyVGrid(
            columns: [GridItem(.adaptive(minimum: 112, maximum: 260), spacing: 8, alignment: .leading)],
            alignment: .leading,
            spacing: 8
        ) {
            ForEach(choices) { choice in
                Button {
                    model.choose(question: question, option: choice.index, label: choice.label)
                } label: {
                    Text(choice.label)
                        .font(.system(size: 12.5, weight: .medium))
                        .foregroundStyle(Theme.inkSoft)
                        .lineLimit(2)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .background(Theme.surface, in: Capsule())
                        .overlay(Capsule().strokeBorder(Theme.line))
                }
                .buttonStyle(.plain)
                .disabled(model.busy)
            }
        }
        .frame(maxWidth: 640, alignment: .leading)
    }
}
