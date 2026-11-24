import CoreGraphics
import Foundation
import ImageIO
import ScreenCaptureKit
import UniformTypeIdentifiers

private struct Request: Decodable {
    let source: String?
    let inspect: Bool?
    let action: InputAction?
    let capture: Bool?
}

private struct Source: Encodable {
    let id: String
    let app: String?
    let title: String?
    let width: Int
    let height: Int
}

struct ScreenError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

@main
struct Screen {
    @MainActor
    static func main() async {
        do {
            let request = try JSONDecoder().decode(
                Request.self, from: FileHandle.standardInput.readDataToEndOfFile()
            )
            try FileHandle.standardOutput.write(contentsOf: await run(request))
        } catch {
            FileHandle.standardError.write(Data("\(error.localizedDescription)\n".utf8))
            exit(1)
        }
    }

    @MainActor
    private static func run(_ request: Request) async throws -> Data {
        if request.inspect == true && (request.action != nil || request.capture == true) {
            throw ScreenError(message: "inspect 需要单独调用")
        }
        if request.capture == true && (request.source == nil || request.action == nil) {
            throw ScreenError(message: "操作后截图需要 source 和 action")
        }
        if let action = request.action, request.source == nil {
            try action.perform(frame: nil, window: nil)
            return Data("已发送输入事件".utf8)
        }
        if request.inspect == true && request.source == nil {
            throw ScreenError(message: "读取控件需要指定窗口 source")
        }
        guard CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess() else {
            throw ScreenError(message: "请在系统设置的「屏幕与系统音频录制」中授权，然后重新调用")
        }
        let content = try await SCShareableContent.excludingDesktopWindows(
            true, onScreenWindowsOnly: true
        )
        guard let source = request.source else {
            let sources = content.displays.map { display in
                Source(id: "display:\(display.displayID)", app: nil, title: nil,
                       width: display.width, height: display.height)
            } + content.windows.map { window in
                Source(id: "window:\(window.windowID)", app: window.owningApplication?.applicationName,
                       title: window.title, width: Int(window.frame.width), height: Int(window.frame.height))
            }
            return try JSONEncoder().encode(sources)
        }

        let target = try CaptureTarget(source, in: content)
        if request.inspect == true {
            guard let window = target.window else {
                throw ScreenError(message: "读取控件需要窗口 source，请先列出窗口")
            }
            return try inspectWindow(window)
        }
        guard let action = request.action else { return try await capture(target.filter) }
        try action.perform(frame: target.frame, window: target.window)
        guard request.capture == true else { return Data("已发送输入事件".utf8) }
        do {
            try await Task.sleep(for: .milliseconds(180))
            let updated = try await SCShareableContent.excludingDesktopWindows(
                true, onScreenWindowsOnly: true
            )
            return try await capture(CaptureTarget(source, in: updated).filter)
        } catch {
            return Data("已发送输入事件，刷新画面失败：\(error.localizedDescription)".utf8)
        }
    }

    private static func capture(_ filter: SCContentFilter) async throws -> Data {
        let info = SCShareableContent.info(for: filter)
        let size = info.contentRect.size
        guard size.width > 0, size.height > 0 else {
            throw ScreenError(message: "取景对象没有可捕捉的画面")
        }
        let scale = min(CGFloat(info.pointPixelScale), 1600 / max(size.width, size.height))
        let configuration = SCStreamConfiguration()
        configuration.width = max(1, Int((size.width * scale).rounded()))
        configuration.height = max(1, Int((size.height * scale).rounded()))
        configuration.showsCursor = false
        configuration.ignoreShadowsSingleWindow = true

        let image = try await SCScreenshotManager.captureImage(
            contentFilter: filter, configuration: configuration
        )
        for quality in [0.75, 0.5, 0.25] {
            let jpeg = NSMutableData()
            guard let encoder = CGImageDestinationCreateWithData(
                jpeg, UTType.jpeg.identifier as CFString, 1, nil
            ) else { break }
            CGImageDestinationAddImage(
                encoder, image, [kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary
            )
            if CGImageDestinationFinalize(encoder), jpeg.length <= 128 * 1024 {
                return jpeg as Data
            }
        }
        throw ScreenError(message: "画面无法编码为 128 KiB 以内的 JPEG")
    }
}

private struct CaptureTarget {
    let filter: SCContentFilter
    let frame: CGRect
    let window: SCWindow?

    init(_ source: String, in content: SCShareableContent) throws {
        if let display = content.displays.first(where: { "display:\($0.displayID)" == source }) {
            filter = SCContentFilter(display: display, excludingWindows: [])
            frame = CGDisplayBounds(display.displayID)
            window = nil
        } else if let selected = content.windows.first(where: { "window:\($0.windowID)" == source }) {
            filter = SCContentFilter(desktopIndependentWindow: selected)
            frame = selected.frame
            window = selected
        } else {
            throw ScreenError(message: "取景对象已不可用，请重新列出显示器和窗口")
        }
    }
}
