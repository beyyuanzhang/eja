import AppKit
import EjaClient
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    var onTerminate: (() -> Void)?
    var safeMode = ProcessInfo.processInfo.environment["EJA_SAFE_MODE"] == "1"
        || NSEvent.modifierFlags.contains(.option)
    private var uiWatch: DispatchSourceFileSystemObject?

    func applicationDidFinishLaunching(_ notification: Notification) {
        if NSEvent.modifierFlags.contains(.option) {
            safeMode = true
        }
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
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

@main
struct EjaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var client = HostClient()
    @StateObject private var ui = ContentUIState()

    var body: some Scene {
        Window("EJA", id: "main") {
            ContentView(client: client, ui: ui)
                .frame(minWidth: 520, minHeight: 460)
                .onAppear {
                    let hostClient = client
                    appDelegate.onTerminate = { [weak hostClient] in
                        Task { @MainActor in
                            hostClient?.shutdown(wait: false)
                        }
                    }
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
                    ui.showSettingsSheet = true
                }
                .keyboardShortcut(",", modifiers: .command)
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
