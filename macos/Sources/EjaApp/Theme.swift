import Combine
import AppKit
import SwiftUI

enum EjaTheme {
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

struct EjaPrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        BodyView(configuration: configuration)
    }

    private struct BodyView: View {
        let configuration: Configuration
        @Environment(\.isEnabled) private var isEnabled
        @StateObject private var hover = HoverState()

        var body: some View {
            configuration.label
                .font(.system(size: 12.5, weight: .medium))
                .foregroundStyle(EjaTheme.canvas)
                .padding(.horizontal, 13)
                .padding(.vertical, 6)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(EjaTheme.ink.opacity(hover.hovered ? 0.88 : 1.0))
                )
                .scaleEffect(configuration.isPressed ? 0.97 : 1.0)
                .opacity(isEnabled ? 1.0 : 0.35)
                .animation(.easeOut(duration: 0.14), value: configuration.isPressed)
                .animation(.easeOut(duration: 0.14), value: hover.hovered)
                .onHover { hover.hovered = $0 }
        }
    }
}

struct EjaSecondaryButtonStyle: ButtonStyle {
    var destructive: Bool = false
    var bordered: Bool = false

    func makeBody(configuration: Configuration) -> some View {
        BodyView(configuration: configuration, destructive: destructive, bordered: bordered)
    }

    private struct BodyView: View {
        let configuration: Configuration
        let destructive: Bool
        let bordered: Bool
        @Environment(\.isEnabled) private var isEnabled
        @StateObject private var hover = HoverState()

        var body: some View {
            let fg: Color = destructive
                ? (hover.hovered ? EjaTheme.danger : EjaTheme.inkSecondary)
                : (hover.hovered ? EjaTheme.ink : EjaTheme.inkSecondary)
            let bg: Color = {
                if destructive && hover.hovered { return EjaTheme.dangerTint }
                if hover.hovered { return EjaTheme.surfaceSubtle }
                return bordered ? EjaTheme.surface : .clear
            }()
            configuration.label
                .font(.system(size: 12.5, weight: .medium))
                .foregroundStyle(fg)
                .padding(.horizontal, 10)
                .padding(.vertical, 5.5)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(bg)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .stroke(bordered ? EjaTheme.hairline : .clear, lineWidth: 1)
                )
                .scaleEffect(configuration.isPressed ? 0.97 : 1.0)
                .opacity(isEnabled ? 1.0 : 0.38)
                .animation(.easeOut(duration: 0.14), value: configuration.isPressed)
                .animation(.easeOut(duration: 0.14), value: hover.hovered)
                .onHover { hover.hovered = $0 }
        }
    }
}

struct EjaIconButtonStyle: ButtonStyle {
    var size: CGFloat = 28

    func makeBody(configuration: Configuration) -> some View {
        BodyView(configuration: configuration, size: size)
    }

    private struct BodyView: View {
        let configuration: Configuration
        let size: CGFloat
        @StateObject private var hover = HoverState()

        var body: some View {
            configuration.label
                .foregroundStyle(hover.hovered ? EjaTheme.ink : EjaTheme.inkSecondary)
                .frame(width: size, height: size)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(hover.hovered ? EjaTheme.surfaceSubtle : .clear)
                )
                .scaleEffect(configuration.isPressed ? 0.95 : 1.0)
                .animation(.easeOut(duration: 0.14), value: configuration.isPressed)
                .animation(.easeOut(duration: 0.14), value: hover.hovered)
                .onHover { hover.hovered = $0 }
        }
    }
}

struct EjaFieldContainer: ViewModifier {
    var focused: Bool
    var cornerRadius: CGFloat = 7
    var horizontalPadding: CGFloat = 9
    var verticalPadding: CGFloat = 6

    func body(content: Content) -> some View {
        content
            .padding(.horizontal, horizontalPadding)
            .padding(.vertical, verticalPadding)
            .background(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .fill(EjaTheme.surface)
            )
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .stroke(focused ? EjaTheme.hairlineStrong : EjaTheme.hairline, lineWidth: 1)
            )
            .animation(.easeOut(duration: 0.15), value: focused)
    }
}

extension View {
    func ejaField(
        focused: Bool = false,
        cornerRadius: CGFloat = 7,
        horizontalPadding: CGFloat = 9,
        verticalPadding: CGFloat = 6
    ) -> some View {
        modifier(
            EjaFieldContainer(
                focused: focused,
                cornerRadius: cornerRadius,
                horizontalPadding: horizontalPadding,
                verticalPadding: verticalPadding
            )
        )
    }
}
