import Foundation
import SwiftUI

/// Chat text keeps its written formatting without exposing Markdown source.
struct MarkdownText: View {
    let text: String

    var body: some View {
        Text(Self.render(text))
    }

    private final class CachedText {
        let value: AttributedString

        init(_ value: AttributedString) {
            self.value = value
        }
    }

    private static let cache: NSCache<NSString, CachedText> = {
        let cache = NSCache<NSString, CachedText>()
        cache.countLimit = 128
        return cache
    }()

    private static func render(_ text: String) -> AttributedString {
        let key = text as NSString
        if let cached = cache.object(forKey: key) {
            return cached.value
        }
        let rendered = (try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        )) ?? AttributedString(text)
        cache.setObject(CachedText(rendered), forKey: key)
        return rendered
    }
}
