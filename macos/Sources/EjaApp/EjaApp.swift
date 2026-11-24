import AppKit
import EjaClient
import SwiftUI

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    var onTerminate: (() -> Void)?
    var reopenMainWindow: (() -> Void)?
    weak var mainWindow: NSWindow?
    var safeMode = ProcessInfo.processInfo.environment["EJA_SAFE_MODE"] == "1"
        || NSEvent.modifierFlags.contains(.option)
    private var uiWatch: DispatchSourceFileSystemObject?
    private var windowObserver: NSObjectProtocol?

    func applicationDidFinishLaunching(_ notification: Notification) {
        if NSEvent.modifierFlags.contains(.option) {
            safeMode = true
        }
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        windowObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification,
            object: nil,
            queue: .main
        ) { [weak self] note in
            guard let window = note.object as? NSWindow else { return }
            Task { @MainActor in
                self?.bindMainWindowIfNeeded(window)
            }
        }
        DispatchQueue.main.async { [weak self] in
            self?.captureExistingMainWindow()
        }
    }

    func captureExistingMainWindow() {
        if let win = NSApp.windows.first(where: { $0.canBecomeMain }) {
            bindMainWindowIfNeeded(win)
        }
    }

    func bindMainWindowIfNeeded(_ window: NSWindow) {
        guard window.canBecomeMain else { return }
        mainWindow = window
        window.isReleasedWhenClosed = false
        if let closeBtn = window.standardWindowButton(.closeButton) {
            closeBtn.target = self
            closeBtn.action = #selector(hideMainWindow(_:))
        }
    }

    @objc func hideMainWindow(_ sender: Any?) {
        captureExistingMainWindow()
        if let win = mainWindow ?? NSApp.keyWindow {
            win.orderOut(nil)
        }
    }

    func showMainWindow() {
        captureExistingMainWindow()
        if let win = mainWindow {
            if win.isMiniaturized {
                win.deminiaturize(nil)
            }
            win.makeKeyAndOrderFront(nil)
        } else {
            reopenMainWindow?()
        }
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag {
            showMainWindow()
        }
        return true
    }

    func applicationDidBecomeActive(_ notification: Notification) {
        let hasVisible = NSApp.windows.contains { $0.isVisible && $0.canBecomeMain }
        if !hasVisible {
            showMainWindow()
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func watchUI(at directory: URL, onChange: @escaping @MainActor () -> Void) {
        guard uiWatch == nil else { return }
        let fd = open(directory.path, O_EVTONLY)
        guard fd >= 0 else { return }
        let watch = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: fd, eventMask: .write, queue: .main
        )
        watch.setEventHandler {
            Task { @MainActor in onChange() }
        }
        watch.setCancelHandler { close(fd) }
        uiWatch = watch
        watch.resume()
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let windowObserver {
            NotificationCenter.default.removeObserver(windowObserver)
        }
        uiWatch?.cancel()
        onTerminate?()
    }
}

@_cdecl("eja_ui_main")
public func eja_ui_main() {
    EjaApp.main()
}

struct EjaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @Environment(\.openWindow) private var openWindow
    @StateObject private var client = HostClient()
    @StateObject private var ui = ContentUIState()

    var body: some Scene {
        Window("EJA", id: "main") {
            ContentView(client: client, ui: ui)
                .frame(minWidth: 280, minHeight: 220)
                .onAppear {
                    let hostClient = client
                    appDelegate.onTerminate = { [weak hostClient] in
                        Task { @MainActor in
                            hostClient?.shutdown(wait: false)
                        }
                    }
                    appDelegate.reopenMainWindow = { [openWindow] in
                        openWindow(id: "main")
                        for window in NSApp.windows where window.canBecomeMain {
                            if window.isMiniaturized {
                                window.deminiaturize(nil)
                            }
                            window.makeKeyAndOrderFront(nil)
                        }
                    }
                    appDelegate.captureExistingMainWindow()
                    bootstrapUI(dataDirPath: client.dataDir)
                    handleAlreadyRunning()
                }
                .onChange(of: client.dataDir) { _, newDir in
                    bootstrapUI(dataDirPath: newDir)
                }
                .onChange(of: client.status) { _, newStatus in
                    if newStatus == "idle", !client.dataDir.isEmpty {
                        checkHotReload(in: URL(fileURLWithPath: client.dataDir, isDirectory: true))
                    }
                }
                .onChange(of: client.alreadyRunning) { _, _ in
                    handleAlreadyRunning()
                }
        }
        .defaultSize(width: 780, height: 600)
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(replacing: .appSettings) {
                Button("设置…") {
                    appDelegate.showMainWindow()
                    ui.showSettingsSheet.toggle()
                }
                .keyboardShortcut(",", modifiers: .command)
            }
            CommandGroup(replacing: .saveItem) {
                Button("关闭窗口") {
                    if ui.showSettingsSheet {
                        ui.showSettingsSheet = false
                    } else {
                        appDelegate.hideMainWindow(nil)
                    }
                }
                .keyboardShortcut("w", modifiers: .command)
            }
        }
    }

    @MainActor
    private func handleAlreadyRunning() {
        guard client.alreadyRunning else { return }
        if let pid = client.alreadyRunningPid,
           let running = NSRunningApplication(processIdentifier: pid) {
            running.activate()
        }
        client.shutdown(wait: false)
        NSApp.terminate(nil)
    }

    @MainActor
    private func bootstrapUI(dataDirPath: String) {
        guard !dataDirPath.isEmpty else { return }
        let dataDir = URL(fileURLWithPath: dataDirPath, isDirectory: true)
        if appDelegate.safeMode {
            UiRuntime.restoreFactoryIfNeeded(in: dataDir) {
                client.shutdown(wait: true)
            }
            return
        }
        guard UiRuntime.bootstrapIfNeeded(dataDir: dataDir) else { return }
        appDelegate.watchUI(at: dataDir.appendingPathComponent("ui")) {
            checkHotReload(in: dataDir)
        }
        checkHotReload(in: dataDir)
    }

    @MainActor
    private func checkHotReload(in dataDir: URL) {
        guard !appDelegate.safeMode,
              client.isReady,
              client.status == "idle",
              UiRuntime.needsHotSwap(in: dataDir) else { return }
        UiRuntime.installAndRelaunch(in: dataDir, baselineId: ui.baselineId) {
            client.shutdown(wait: true)
        }
    }
}
