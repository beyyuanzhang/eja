import ApplicationServices
import AppKit
import ScreenCaptureKit

let accessibilityError = ScreenError(message: "请在「系统设置 → 隐私与安全性 → 辅助功能」授权 EJA，再调用此工具")

private struct Control: Encodable {
    let role: String
    let label: String?
    let value: String?
    let x: Double
    let y: Double
}

private struct Inspection: Encodable {
    let elements: [Control]
    let truncated: Bool
}

@MainActor
func isFocused(_ window: SCWindow) -> Bool {
    guard let pid = window.owningApplication?.processID,
          NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else { return false }
    let app = application(pid)
    guard let element = elementAttribute(app, kAXFocusedWindowAttribute) else { return false }
    return matches(element, window: window)
}

func isExposed(_ window: SCWindow, at point: CGPoint) -> Bool {
    let desktop = AXUIElementCreateSystemWide()
    AXUIElementSetMessagingTimeout(desktop, 0.25)
    var hit: AXUIElement?
    guard AXUIElementCopyElementAtPosition(desktop, Float(point.x), Float(point.y), &hit) == .success,
          let hit else { return false }
    var pid: pid_t = 0
    guard AXUIElementGetPid(hit, &pid) == .success, pid == window.owningApplication?.processID else {
        return false
    }
    return matches(elementAttribute(hit, kAXWindowAttribute) ?? hit, window: window)
}

func inspectWindow(_ window: SCWindow) throws -> Data {
    guard AXIsProcessTrustedWithOptions(
        [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
    ) else { throw accessibilityError }
    let windows = window.owningApplication.map { children(application($0.processID), kAXWindowsAttribute) } ?? []
    let matching = windows.filter { matches($0, window: window) }
    guard matching.count == 1, let root = matching.first else {
        throw ScreenError(message: "没有读到对应窗口的辅助功能内容，可以使用截图")
    }

    let clock = ContinuousClock()
    let deadline = clock.now.advanced(by: .seconds(2))
    var pending = [root]
    var elements: [Control] = []
    var visited = 0
    var limited = false
    while visited < 160, elements.count < 48, clock.now < deadline, let element = pending.popLast() {
        visited += 1
        if attribute(element, kAXSubroleAttribute) as? String == kAXSecureTextFieldSubrole { continue }
        let descendants = children(element, kAXChildrenAttribute)
        limited = limited || descendants.count == 48
        pending.append(contentsOf: descendants.reversed())
        guard let role = attribute(element, kAXRoleAttribute) as? String,
              role != kAXWindowRole, let rect = bounds(of: element) else { continue }
        let visible = rect.intersection(window.frame)
        guard !visible.isEmpty else { continue }
        let label = short(attribute(element, kAXTitleAttribute) as? String, limit: 96)
            ?? short(attribute(element, kAXDescriptionAttribute) as? String, limit: 96)
        let rawValue = attribute(element, kAXValueAttribute)
        let value = short((rawValue as? String) ?? (rawValue as? NSNumber)?.stringValue, limit: 80)
        guard label != nil || value != nil else { continue }
        let x = (visible.midX - window.frame.minX) / window.frame.width
        let y = (visible.midY - window.frame.minY) / window.frame.height
        elements.append(Control(role: role, label: label, value: value == label ? nil : value,
                                x: (x * 10000).rounded() / 10000, y: (y * 10000).rounded() / 10000))
    }
    return try JSONEncoder().encode(Inspection(elements: elements, truncated: limited || !pending.isEmpty))
}

private func application(_ pid: pid_t) -> AXUIElement {
    let app = AXUIElementCreateApplication(pid)
    AXUIElementSetMessagingTimeout(app, 0.25)
    return app
}

private func matches(_ element: AXUIElement, window: SCWindow) -> Bool {
    guard let rect = bounds(of: element) else { return false }
    let frame = window.frame
    guard abs(rect.minX - frame.minX) <= 3, abs(rect.minY - frame.minY) <= 3,
          abs(rect.width - frame.width) <= 3, abs(rect.height - frame.height) <= 3 else { return false }
    if let title = window.title, !title.isEmpty,
       let candidate = attribute(element, kAXTitleAttribute) as? String, !candidate.isEmpty {
        return candidate == title
    }
    return true
}

private func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}

private func elementAttribute(_ element: AXUIElement, _ name: String) -> AXUIElement? {
    guard let value = attribute(element, name), CFGetTypeID(value) == AXUIElementGetTypeID() else { return nil }
    return (value as! AXUIElement)
}

private func children(_ element: AXUIElement, _ name: String) -> [AXUIElement] {
    var values: CFArray?
    guard AXUIElementCopyAttributeValues(element, name as CFString, 0, 48, &values) == .success else { return [] }
    return values as? [AXUIElement] ?? []
}

private func bounds(of element: AXUIElement) -> CGRect? {
    guard let position = attribute(element, kAXPositionAttribute), CFGetTypeID(position) == AXValueGetTypeID(),
          let size = attribute(element, kAXSizeAttribute), CFGetTypeID(size) == AXValueGetTypeID() else { return nil }
    var origin = CGPoint.zero
    var dimensions = CGSize.zero
    guard AXValueGetValue(position as! AXValue, .cgPoint, &origin),
          AXValueGetValue(size as! AXValue, .cgSize, &dimensions) else { return nil }
    return CGRect(origin: origin, size: dimensions)
}

private func short(_ value: String?, limit: Int) -> String? {
    guard let value, !value.isEmpty else { return nil }
    return String(value.unicodeScalars.prefix(limit))
}
