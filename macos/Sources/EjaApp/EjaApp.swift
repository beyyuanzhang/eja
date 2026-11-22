import AppKit
import EjaClient
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    var onTerminate: (() -> Void)?
    var runningStamp: BinaryStamp?
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

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        ProcessInfo.processInfo.environment["EJA_ATTACHED_UI"] == "1"
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
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some Scene {
        Window("EJA", id: "main") {
            ContentView(client: client)
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
                .onChange(of: client.panelPid) { old, pid in
                    if pid != nil {
                        NSApp.hide(nil)
                    } else if old != nil {
                        NSApp.unhide(nil)
                        NSApp.activate(ignoringOtherApps: true)
                    }
                }
                .onChange(of: client.isReady) { wasReady, ready in
                    if client.isAttachedUI && wasReady && !ready {
                        NSApp.terminate(nil)
                    }
                }
                .onChange(of: client.settingsRequests) { _, _ in
                    NSApp.activate(ignoringOtherApps: true)
                    openSettings()
                }
                .onChange(of: client.alreadyRunning) { _, _ in
                    handleAlreadyRunning()
                }
        }
        .defaultSize(width: 780, height: 600)
        .windowStyle(.hiddenTitleBar)

        Settings {
            SettingsView(client: client)
        }

        MenuBarExtra(isInserted: .constant(!client.isAttachedUI)) {
            Text(client.kernelVersion.isEmpty ? "EJA · \(client.statusDetail)" : "EJA \(client.kernelVersion) · \(client.statusDetail)")
            if let nextWake = client.wakes.first {
                Text("下次唤醒：\(nextWake.fireAtDisplay)")
            }
            if !client.concerns.isEmpty {
                Divider()
                Text("挂念 (\(client.concerns.count))")
                ForEach(client.concerns) { concern in
                    Text("• \(concern.text)")
                }
            }
            Divider()
            Button("打开 EJA") {
                if let pid = client.panelPid,
                   let panel = NSRunningApplication(processIdentifier: pid) {
                    panel.activate()
                } else {
                    NSApp.activate(ignoringOtherApps: true)
                    openWindow(id: "main")
                }
            }
            .keyboardShortcut("o")
            Divider()
            Button("退出 EJA") {
                client.shutdown(wait: false)
                NSApp.terminate(nil)
            }
            .keyboardShortcut("q")
        } label: {
            Image(systemName: client.status == "thinking" ? "sparkles" : "circle.inset.filled")
                .accessibilityLabel("EJA")
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
        guard !client.isAttachedUI, !dataDirPath.isEmpty, !appDelegate.safeMode else { return }
        let dataDir = URL(fileURLWithPath: dataDirPath, isDirectory: true)
        guard UiRuntime.bootstrapIfNeeded(dataDir: dataDir) else { return }
        appDelegate.watchUI(at: dataDir.appendingPathComponent("ui")) {
            checkHotReload(in: dataDir)
        }
        checkHotReload(in: dataDir)
    }

    @MainActor
    private func checkHotReload(in dataDir: URL) {
        let target = UiRuntime.evolvedExecutable(in: dataDir)
        guard FileManager.default.isExecutableFile(atPath: target.path),
              let stamp = UiRuntime.binaryStamp(at: target),
              stamp != appDelegate.runningStamp else { return }
        appDelegate.runningStamp = stamp
        client.launchUserInterface()
    }
}
