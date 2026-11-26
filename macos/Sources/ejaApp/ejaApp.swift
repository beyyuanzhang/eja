import AppKit
import ejaClient
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    var onTerminate: (() -> Void)?
    var reopenWindow: (() -> Void)?
    let safeMode = ProcessInfo.processInfo.environment["eja_SAFE_MODE"] == "1"
    private var uiWatch: DispatchSourceFileSystemObject?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        guard !flag, let reopenWindow else { return true }
        reopenWindow()
        return false
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
        uiWatch?.cancel()
        onTerminate?()
    }
}

@_cdecl("eja_ui_main")
public func eja_ui_main() {
    ejaApp.main()
}

struct ejaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @Environment(\.openWindow) private var openWindow
    @StateObject private var client = HostClient()
    @StateObject private var ui = ContentUIState()

    private var windowSize: CGSize {
        let screen = NSScreen.main?.visibleFrame.size ?? .zero
        return CGSize(width: screen.width * 0.5, height: screen.height * 0.65)
    }

    var body: some Scene {
        Window("eja", id: "main") {
            ContentView(client: client, ui: ui)
        }
        .defaultSize(windowSize)
        .windowStyle(.hiddenTitleBar)
        .onChange(of: client.dataDir, initial: true) { _, newDir in
            if appDelegate.reopenWindow == nil {
                let hostClient = client
                appDelegate.onTerminate = { [weak hostClient] in
                    Task { @MainActor in hostClient?.shutdown(wait: false) }
                }
                appDelegate.reopenWindow = { openWindow(id: "main") }
                openWindow(id: "main")
            }
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
        .commands {
            CommandGroup(replacing: .appSettings) {
                Button("设置…") {
                    withAnimation(.smooth) { ui.settingsOpen.toggle() }
                }
                .keyboardShortcut(",", modifiers: .command)
                .disabled(!client.isReady)
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
        guard !appDelegate.safeMode else { return }
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
              client.status == "idle" else { return }
        UiRuntime.reloadIfNeeded(in: dataDir, baselineId: ui.baselineId) {
            client.shutdown(wait: true)
        }
    }
}
