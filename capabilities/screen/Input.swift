import Carbon.HIToolbox
import CoreGraphics
import Foundation
import ScreenCaptureKit

struct InputAction: Decodable {
    enum Kind: String, Decodable {
        case move, click, drag, scroll, key, type
    }

    enum Button: String, Decodable {
        case left, right
    }

    struct Point: Decodable {
        let x: CGFloat
        let y: CGFloat
    }

    struct Delta: Decodable {
        let x: Int32?
        let y: Int32?
    }

    let type: Kind
    let at: Point?
    let to: Point?
    let button: Button?
    let count: Int?
    let delta: Delta?
    let key: String?
    let text: String?

    @MainActor
    func perform(frame: CGRect?, window: SCWindow?) throws {
        let events: [CGEvent]
        var points: [CGPoint] = []
        switch type {
        case .key:
            events = try keyEvents(key ?? "")
        case .type:
            events = try textEvents(text ?? "")
        case .move, .click, .drag, .scroll:
            let start = try position(at, in: frame)
            points.append(start)
            switch type {
            case .move:
                events = [try mouse(.mouseMoved, at: start)]
            case .click:
                let clicks = count ?? 1
                guard (1...2).contains(clicks) else {
                    throw ScreenError(message: "click 的 count 为 1 或 2")
                }
                let right = button == .right
                let mouseButton: CGMouseButton = right ? .right : .left
                events = try (1...clicks).flatMap { click in
                    try [mouse(right ? .rightMouseDown : .leftMouseDown, at: start, button: mouseButton, count: click),
                         mouse(right ? .rightMouseUp : .leftMouseUp, at: start, button: mouseButton, count: click)]
                }
            case .drag:
                let end = try position(to, in: frame)
                points.append(end)
                let path = try (1...8).map { step in
                    let progress = CGFloat(step) / 8
                    return try mouse(.leftMouseDragged, at: CGPoint(
                        x: start.x + (end.x - start.x) * progress,
                        y: start.y + (end.y - start.y) * progress
                    ), count: 1)
                }
                events = [try mouse(.leftMouseDown, at: start, count: 1)] + path
                    + [try mouse(.leftMouseUp, at: end, count: 1)]
            case .scroll:
                let horizontal = delta?.x ?? 0
                let vertical = delta?.y ?? 0
                guard (horizontal != 0 || vertical != 0), horizontal != .min, vertical != .min,
                      let event = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2,
                                          wheel1: -vertical, wheel2: -horizontal, wheel3: 0) else {
                    throw ScreenError(message: "scroll 需要非零整数 delta")
                }
                event.location = start
                events = [event]
            default:
                preconditionFailure()
            }
        }
        guard CGPreflightPostEventAccess() || CGRequestPostEventAccess() else {
            throw accessibilityError
        }
        if let window {
            if points.isEmpty {
                guard isFocused(window) else {
                    throw ScreenError(message: "窗口未取得键盘焦点，请先在画面中点击目标")
                }
            } else if !points.allSatisfy({ isExposed(window, at: $0) }) {
                throw ScreenError(message: "无法确认该坐标命中所选窗口，请从显示器画面操作")
            }
        }
        for event in events {
            event.post(tap: .cghidEventTap)
            if type == .drag { Thread.sleep(forTimeInterval: 0.015) }
        }
    }

    private func position(_ point: Point?, in frame: CGRect?) throws -> CGPoint {
        guard let frame, frame.width >= 1, frame.height >= 1, let point,
              (0...1).contains(point.x), (0...1).contains(point.y) else {
            throw ScreenError(message: "鼠标操作需要 source 和 0 到 1 之间的 x、y")
        }
        let location = CGPoint(x: frame.minX + min(frame.width * point.x, frame.width - 1),
                               y: frame.minY + min(frame.height * point.y, frame.height - 1))
        var display: CGDirectDisplayID = 0
        var count: UInt32 = 0
        guard CGGetDisplaysWithPoint(location, 1, &display, &count) == .success, count > 0 else {
            throw ScreenError(message: "目标坐标不在显示器内")
        }
        return location
    }

    private func mouse(
        _ kind: CGEventType, at point: CGPoint, button: CGMouseButton = .left, count: Int = 0
    ) throws -> CGEvent {
        guard let event = CGEvent(mouseEventSource: nil, mouseType: kind,
                                  mouseCursorPosition: point, mouseButton: button) else {
            throw ScreenError(message: "无法创建鼠标事件")
        }
        event.setIntegerValueField(.mouseEventClickState, value: Int64(count))
        return event
    }

    private func keyEvents(_ shortcut: String) throws -> [CGEvent] {
        let parts = shortcut.lowercased().split(separator: "+", omittingEmptySubsequences: false)
        guard let key = parts.last, let code = Self.keys[String(key)] else {
            throw ScreenError(message: "不支持的按键：\(shortcut)")
        }
        var flags: CGEventFlags = []
        for modifier in parts.dropLast() {
            switch modifier {
            case "cmd": flags.insert(.maskCommand)
            case "shift": flags.insert(.maskShift)
            case "ctrl": flags.insert(.maskControl)
            case "option": flags.insert(.maskAlternate)
            default: throw ScreenError(message: "不支持的修饰键：\(modifier)")
            }
        }
        return try keyPair(CGKeyCode(code), flags: flags)
    }

    private func textEvents(_ text: String) throws -> [CGEvent] {
        guard !text.isEmpty, text.utf16.count <= 4096 else {
            throw ScreenError(message: "type 需要 1 到 4096 个 UTF-16 代码单元")
        }
        var events: [CGEvent] = []
        var units: [UInt16] = []
        func flush() throws {
            let pair = try keyPair(0)
            for event in pair {
                units.withUnsafeBufferPointer {
                    event.keyboardSetUnicodeString(stringLength: $0.count, unicodeString: $0.baseAddress)
                }
            }
            events.append(contentsOf: pair)
            units.removeAll(keepingCapacity: true)
        }
        for scalar in text.unicodeScalars {
            let next = String(scalar).utf16
            if units.count + next.count > 20 { try flush() }
            units.append(contentsOf: next)
        }
        if !units.isEmpty { try flush() }
        return events
    }

    private func keyPair(_ code: CGKeyCode, flags: CGEventFlags = []) throws -> [CGEvent] {
        try [true, false].map { pressed in
            guard let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: pressed) else {
                throw ScreenError(message: "无法创建键盘事件")
            }
            event.flags = flags
            return event
        }
    }

    private static let keys: [String: Int] = [
        "a": kVK_ANSI_A, "b": kVK_ANSI_B, "c": kVK_ANSI_C, "d": kVK_ANSI_D,
        "e": kVK_ANSI_E, "f": kVK_ANSI_F, "g": kVK_ANSI_G, "h": kVK_ANSI_H,
        "i": kVK_ANSI_I, "j": kVK_ANSI_J, "k": kVK_ANSI_K, "l": kVK_ANSI_L,
        "m": kVK_ANSI_M, "n": kVK_ANSI_N, "o": kVK_ANSI_O, "p": kVK_ANSI_P,
        "q": kVK_ANSI_Q, "r": kVK_ANSI_R, "s": kVK_ANSI_S, "t": kVK_ANSI_T,
        "u": kVK_ANSI_U, "v": kVK_ANSI_V, "w": kVK_ANSI_W, "x": kVK_ANSI_X,
        "y": kVK_ANSI_Y, "z": kVK_ANSI_Z,
        "0": kVK_ANSI_0, "1": kVK_ANSI_1, "2": kVK_ANSI_2, "3": kVK_ANSI_3,
        "4": kVK_ANSI_4, "5": kVK_ANSI_5, "6": kVK_ANSI_6, "7": kVK_ANSI_7,
        "8": kVK_ANSI_8, "9": kVK_ANSI_9,
        "return": kVK_Return, "enter": kVK_Return, "tab": kVK_Tab, "space": kVK_Space,
        "esc": kVK_Escape, "escape": kVK_Escape, "backspace": kVK_Delete, "delete": kVK_ForwardDelete,
        "up": kVK_UpArrow, "down": kVK_DownArrow, "left": kVK_LeftArrow, "right": kVK_RightArrow,
        "home": kVK_Home, "end": kVK_End, "pageup": kVK_PageUp, "pagedown": kVK_PageDown,
        "f1": kVK_F1, "f2": kVK_F2, "f3": kVK_F3, "f4": kVK_F4, "f5": kVK_F5, "f6": kVK_F6,
        "f7": kVK_F7, "f8": kVK_F8, "f9": kVK_F9, "f10": kVK_F10, "f11": kVK_F11, "f12": kVK_F12,
        ",": kVK_ANSI_Comma, ".": kVK_ANSI_Period, "-": kVK_ANSI_Minus, "=": kVK_ANSI_Equal,
        "/": kVK_ANSI_Slash, "[": kVK_ANSI_LeftBracket, "]": kVK_ANSI_RightBracket,
    ]
}
