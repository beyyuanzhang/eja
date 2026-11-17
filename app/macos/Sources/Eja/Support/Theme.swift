import SwiftUI

enum Theme {
    static let paper = Color(red: 0xF8 / 255, green: 0xF7 / 255, blue: 0xF2 / 255)
    static let surface = Color(red: 0xFF / 255, green: 0xFE / 255, blue: 0xFA / 255)
    static let surface2 = Color(red: 0xF3 / 255, green: 0xF1 / 255, blue: 0xEB / 255)
    static let ink = Color(red: 0x24 / 255, green: 0x23 / 255, blue: 0x1F / 255)
    static let inkSoft = Color(red: 0x44 / 255, green: 0x42 / 255, blue: 0x3C / 255)
    static let muted = Color(red: 0x7D / 255, green: 0x79 / 255, blue: 0x6F / 255)
    static let faint = Color(red: 0xA7 / 255, green: 0xA2 / 255, blue: 0x97 / 255)
    static let line = ink.opacity(0.09)
    static let lineStrong = ink.opacity(0.16)
    static let accent = Color(red: 0xBD / 255, green: 0x60 / 255, blue: 0x42 / 255)
    static let accentDeep = Color(red: 0x96 / 255, green: 0x45 / 255, blue: 0x2D / 255)
    static let accentWash = Color(red: 0xF6 / 255, green: 0xE8 / 255, blue: 0xE1 / 255)
    static let amber = Color(red: 0xA9 / 255, green: 0x77 / 255, blue: 0x2F / 255)
    static let danger = Color(red: 0xA7 / 255, green: 0x46 / 255, blue: 0x36 / 255)
    static let dangerWash = Color(red: 0xF7 / 255, green: 0xE8 / 255, blue: 0xE4 / 255)

    static func serif(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .serif)
    }

    static func mono(_ size: CGFloat) -> Font {
        .system(size: size, design: .monospaced)
    }
}

struct CardBackground: ViewModifier {
    var padding: CGFloat = 14

    func body(content: Content) -> some View {
        content
            .padding(padding)
            .background(Theme.surface, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.line))
    }
}

extension View {
    func card(padding: CGFloat = 14) -> some View {
        modifier(CardBackground(padding: padding))
    }
}
