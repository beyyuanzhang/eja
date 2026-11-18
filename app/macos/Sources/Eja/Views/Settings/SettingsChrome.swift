import SwiftUI

struct SettingsPage<Content: View>: View {
    private let content: Content

    init(@ViewBuilder content: () -> Content) {
        self.content = content()
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 26) {
                content
            }
            .frame(maxWidth: 620, alignment: .leading)
            .padding(.horizontal, 30)
            .padding(.top, 22)
            .padding(.bottom, 40)
            .frame(maxWidth: .infinity)
        }
        .scrollIndicators(.never)
    }
}

struct SettingsGroup<Content: View>: View {
    let title: String
    private let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(Theme.muted)
                .padding(.leading, 2)

            content
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.surface.opacity(0.92), in: RoundedRectangle(cornerRadius: 10))
                .overlay(
                    RoundedRectangle(cornerRadius: 10)
                        .strokeBorder(Theme.line)
                )
        }
    }
}

struct SettingsRow<Content: View>: View {
    let title: String
    let note: String?
    private let content: Content

    init(_ title: String, note: String? = nil, @ViewBuilder content: () -> Content) {
        self.title = title
        self.note = note
        self.content = content()
    }

    var body: some View {
        HStack(alignment: .center, spacing: 18) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 12.5))
                    .foregroundStyle(Theme.inkSoft)
                if let note {
                    Text(note)
                        .font(.system(size: 10.5))
                        .foregroundStyle(Theme.faint)
                }
            }
            Spacer(minLength: 20)
            content
        }
        .padding(.horizontal, 14)
        .frame(minHeight: 44)
    }
}

struct SettingsRule: View {
    var body: some View {
        Divider()
            .overlay(Theme.line)
            .padding(.leading, 14)
    }
}
