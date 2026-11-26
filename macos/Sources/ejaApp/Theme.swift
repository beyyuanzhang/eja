import Combine
import AppKit
import SwiftUI

enum ejaTheme {
    static let canvas = dynamicColor(
        light: NSColor(srgbRed: 0.965, green: 0.961, blue: 0.949, alpha: 1.0), // #F6F5F2
        dark: NSColor(srgbRed: 0.086, green: 0.086, blue: 0.082, alpha: 1.0)   // #161615
    )

    static let surface = dynamicColor(
        light: NSColor(srgbRed: 1.0, green: 1.0, blue: 1.0, alpha: 1.0),
        dark: NSColor(srgbRed: 0.122, green: 0.122, blue: 0.114, alpha: 1.0)   // #1F1F1D
    )

    static let surfaceSubtle = dynamicColor(
        light: NSColor(srgbRed: 0.925, green: 0.918, blue: 0.894, alpha: 1.0), // #ECEAE4
        dark: NSColor(srgbRed: 0.149, green: 0.145, blue: 0.137, alpha: 1.0)    // #262523
    )

    static let ink = dynamicColor(
        light: NSColor(srgbRed: 0.110, green: 0.110, blue: 0.118, alpha: 1.0), // #1C1C1E
        dark: NSColor(srgbRed: 0.949, green: 0.945, blue: 0.925, alpha: 1.0)    // #F2F1EC
    )

    static let inkSecondary = dynamicColor(
        light: NSColor(srgbRed: 0.435, green: 0.427, blue: 0.408, alpha: 1.0), // #6F6D68
        dark: NSColor(srgbRed: 0.612, green: 0.604, blue: 0.576, alpha: 1.0)    // #9C9A93
    )

    static let inkMuted = dynamicColor(
        light: NSColor(srgbRed: 0.635, green: 0.627, blue: 0.604, alpha: 1.0), // #A2A09A
        dark: NSColor(srgbRed: 0.412, green: 0.404, blue: 0.380, alpha: 1.0)    // #696761
    )

    static let hairline = dynamicColor(
        light: NSColor(srgbRed: 0.110, green: 0.110, blue: 0.118, alpha: 0.08),
        dark: NSColor(srgbRed: 1.0, green: 1.0, blue: 1.0, alpha: 0.10)
    )

    static let hairlineStrong = dynamicColor(
        light: NSColor(srgbRed: 0.110, green: 0.110, blue: 0.118, alpha: 0.22),
        dark: NSColor(srgbRed: 1.0, green: 1.0, blue: 1.0, alpha: 0.24)
    )

    static let positive = dynamicColor(
        light: NSColor(srgbRed: 0.184, green: 0.490, blue: 0.298, alpha: 1.0),
        dark: NSColor(srgbRed: 0.361, green: 0.769, blue: 0.514, alpha: 1.0)
    )

    static let warning = dynamicColor(
        light: NSColor(srgbRed: 0.718, green: 0.475, blue: 0.122, alpha: 1.0),
        dark: NSColor(srgbRed: 0.878, green: 0.651, blue: 0.294, alpha: 1.0)
    )

    static let danger = dynamicColor(
        light: NSColor(srgbRed: 0.722, green: 0.231, blue: 0.231, alpha: 1.0),
        dark: NSColor(srgbRed: 0.898, green: 0.420, blue: 0.420, alpha: 1.0)
    )

    static let dangerTint = dynamicColor(
        light: NSColor(srgbRed: 0.722, green: 0.231, blue: 0.231, alpha: 0.07),
        dark: NSColor(srgbRed: 0.898, green: 0.420, blue: 0.420, alpha: 0.12)
    )

    static let subtleShadow = dynamicColor(
        light: NSColor(srgbRed: 0.110, green: 0.110, blue: 0.118, alpha: 0.05),
        dark: NSColor(srgbRed: 0.0, green: 0.0, blue: 0.0, alpha: 0.28)
    )

    private static func dynamicColor(light: NSColor, dark: NSColor) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
        })
    }
}

@MainActor
final class HoverState: ObservableObject {
    @Published var hovered = false
}

struct ejaIconButtonStyle: ButtonStyle {
    var destructive: Bool = false

    func makeBody(configuration: Configuration) -> some View {
        BodyView(configuration: configuration, destructive: destructive)
    }

    private struct BodyView: View {
        let configuration: Configuration
        let destructive: Bool
        @StateObject private var hover = HoverState()

        var body: some View {
            let fg: Color = destructive
                ? (hover.hovered ? ejaTheme.danger : ejaTheme.inkMuted)
                : (hover.hovered ? ejaTheme.ink : ejaTheme.inkSecondary)
            let bg: Color = hover.hovered
                ? (destructive ? ejaTheme.dangerTint : ejaTheme.surfaceSubtle)
                : .clear
            configuration.label
                .foregroundStyle(fg)
                .padding(6)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(bg)
                )
                .scaleEffect(configuration.isPressed ? 0.95 : 1.0)
                .animation(.easeOut(duration: 0.14), value: configuration.isPressed)
                .animation(.easeOut(duration: 0.14), value: hover.hovered)
                .onHover { hover.hovered = $0 }
        }
    }
}
