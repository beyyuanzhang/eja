import SwiftUI

/// Chat text keeps its written formatting without exposing Markdown source.
struct MarkdownText: View {
    let text: String

    var body: some View {
        Text(attributed)
    }

    private var attributed: AttributedString {
        (try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        )) ?? AttributedString(text)
    }
}
