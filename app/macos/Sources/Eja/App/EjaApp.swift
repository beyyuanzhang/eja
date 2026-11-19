import AppKit
import SwiftUI

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = AppModel()
    private var mainWindow: NSWindow?
    private var mainWindowDelegate: MainWindowDelegate?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        model.start()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldHandleReopen(
        _ sender: NSApplication,
        hasVisibleWindows flag: Bool
    ) -> Bool {
        showMainWindow()
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        model.stopRuntime()
    }

    func attach(_ window: NSWindow?) {
        guard let window else { return }
        mainWindow = window
        window.isReleasedWhenClosed = false
        if mainWindowDelegate == nil {
            mainWindowDelegate = MainWindowDelegate()
        }
        if window.delegate !== mainWindowDelegate {
            window.delegate = mainWindowDelegate
        }
    }

    func showMainWindow() {
        NSApp.activate(ignoringOtherApps: true)
        if let mainWindow {
            mainWindow.makeKeyAndOrderFront(nil)
        } else {
            WindowBridge.shared.open?()
        }
    }
}

@MainActor
private final class MainWindowDelegate: NSObject, NSWindowDelegate {
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        sender.orderOut(nil)
        return false
    }
}

@MainActor
final class WindowBridge {
    static let shared = WindowBridge()
    var open: (() -> Void)?
}

@main
struct EjaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @Environment(\.openWindow) private var openWindow

    var body: some Scene {
        Window("eja", id: "main") {
            ContentView(model: appDelegate.model)
                .frame(minWidth: 560, minHeight: 460)
                .preferredColorScheme(.light)
                .background(WindowAccessor { appDelegate.attach($0) })
        }
        .defaultSize(width: 860, height: 700)
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(after: .newItem) {
                Button("刷新") { appDelegate.model.refreshSoon() }
                    .keyboardShortcut("r", modifiers: .command)
            }
            CommandGroup(replacing: .appSettings) {
                Button("设置…") { appDelegate.model.settingsPresented = true }
                    .keyboardShortcut(",", modifiers: .command)
            }
        }

        MenuBarExtra {
            StatusMenu(model: appDelegate.model, openWindow: openMainWindow)
        } label: {
            StatusLabel(model: appDelegate.model)
                .onAppear {
                    WindowBridge.shared.open = openMainWindow
                }
        }
        .menuBarExtraStyle(.menu)
    }

    private func openMainWindow() {
        openWindow(id: "main")
        DispatchQueue.main.async {
            appDelegate.showMainWindow()
        }
    }
}

private struct StatusLabel: View {
    let model: AppModel

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: model.menuIcon)
            if let badge = model.menuBadge {
                Text(badge)
                    .font(.system(size: 10.5, weight: .semibold))
            }
        }
        .help("\(model.statusTitle) · \(model.statusDetail)")
    }
}

private struct StatusMenu: View {
    let model: AppModel
    let openWindow: () -> Void

    var body: some View {
        Text(model.statusTitle)
            .font(.system(size: 12, weight: .semibold))
        Text(model.statusDetail)
            .font(.system(size: 10.5))
            .foregroundStyle(.secondary)

        Divider()

        Button("打开 Eja") {
            openWindow()
        }
        Button("刷新状态") {
            model.refreshSoon()
        }
        Button("重新启动实例") {
            model.restartRuntime()
        }
        .disabled(!model.canRestartRuntime)
        Button("打开运行日志") {
            model.openRuntimeLog()
        }

        Divider()

        Button("退出 Eja") {
            NSApp.terminate(nil)
        }
    }
}

private struct WindowAccessor: NSViewRepresentable {
    let resolve: (NSWindow?) -> Void

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            resolve(view.window)
        }
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        DispatchQueue.main.async {
            resolve(view.window)
        }
    }
}
