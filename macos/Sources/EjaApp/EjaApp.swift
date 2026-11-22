import AppKit
import EjaClient
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    var onTerminate: (() -> Void)?
    var runningStamp: BinaryStamp?
    var uiReady = false
    var isRelaunching = false
    var safeMode = ProcessInfo.processInfo.environment["EJA_SAFE_MODE"] == "1"
        || NSEvent.modifierFlags.contains(.option)
    private var reloadTimer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        if NSEvent.modifierFlags.contains(.option) {
            safeMode = true
        }
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    func startReloadPolling(_ tick: @escaping @MainActor () -> Void) {
        guard reloadTimer == nil else { return }
        reloadTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { _ in
            Task { @MainActor in
                tick()
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationWillTerminate(_ notification: Notification) {
        reloadTimer?.invalidate()
        reloadTimer = nil
        onTerminate?()
    }
}

@main
struct EjaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var client = HostClient()
    @Environment(\.openWindow) private var openWindow

    var body: some Scene {
        Window("EJA", id: "main") {
            ContentView(client: client)
                .frame(minWidth: 520, minHeight: 460)
                .onAppear {
                    let hostClient = client
                    appDelegate.onTerminate = { [weak hostClient] in
                        Task { @MainActor in
                            hostClient?.shutdown()
                        }
                    }
                    bootstrapUI(dataDirPath: client.dataDir)
                    appDelegate.startReloadPolling {
                        checkHotReload()
                    }
                }
                .onChange(of: client.dataDir) { _, newDir in
                    bootstrapUI(dataDirPath: newDir)
                }
        }
        .defaultSize(width: 780, height: 600)

        Settings {
            SettingsView(client: client)
        }

        MenuBarExtra {
            Text(client.kernelVersion.isEmpty ? "EJA · \(client.statusDetail)" : "EJA \(client.kernelVersion) · \(client.statusDetail)")
            if let nextWake = client.wakes.first {
                Text("下次唤醒：\(nextWake.fireAtDisplay)")
            }
            Divider()
            Button("打开 EJA") {
                NSApp.activate(ignoringOtherApps: true)
                openWindow(id: "main")
            }
            .keyboardShortcut("o")
            Button("重置默认界面") {
                resetUI()
            }
            Divider()
            Button("退出 EJA") {
                client.shutdown()
                NSApp.terminate(nil)
            }
            .keyboardShortcut("q")
        } label: {
            Image(systemName: client.status == "thinking" ? "sparkles" : "circle.inset.filled")
                .accessibilityLabel("EJA")
        }
    }

    @MainActor
    private func bootstrapUI(dataDirPath: String) {
        guard !appDelegate.uiReady, !dataDirPath.isEmpty else { return }
        appDelegate.uiReady = true
        let dataDir = URL(fileURLWithPath: dataDirPath, isDirectory: true)
        UiRuntime.bootstrapIfNeeded(dataDir: dataDir)
        if appDelegate.safeMode {
            appDelegate.runningStamp = UiRuntime.binaryStamp(at: UiRuntime.evolvedExecutable(in: dataDir))
            client.errorBanner = "已进入安全模式（官方默认界面），可修改内核设置或重置界面"
        } else {
            appDelegate.runningStamp = UiRuntime.runningStamp(in: dataDir)
            checkHotReload()
        }
    }

    @MainActor
    private func checkHotReload() {
        guard appDelegate.uiReady, !appDelegate.isRelaunching, client.status == "idle", !client.dataDir.isEmpty else { return }
        let dataDir = URL(fileURLWithPath: client.dataDir, isDirectory: true)
        guard let target = UiRuntime.updatedExecutable(in: dataDir, runningStamp: appDelegate.runningStamp) else { return }
        relaunch(to: target, dataDir: dataDir)
    }

    @MainActor
    private func resetUI() {
        guard !client.dataDir.isEmpty else { return }
        let dataDir = URL(fileURLWithPath: client.dataDir, isDirectory: true)
        do {
            if let fallback = try UiRuntime.resetToDefault(dataDir: dataDir) {
                relaunch(to: fallback, dataDir: dataDir)
            }
        } catch {
            client.errorBanner = "重置默认界面失败：\(error.localizedDescription)"
        }
    }

    @MainActor
    private func relaunch(to executable: URL, dataDir: URL) {
        guard !appDelegate.isRelaunching else { return }
        appDelegate.isRelaunching = true
        let hostBin = client.hostExecutableURL
        client.shutdown()
        Task.detached {
            do {
                try UiRuntime.launch(executable: executable, dataDir: dataDir, hostExecutableURL: hostBin)
                await MainActor.run {
                    NSApp.terminate(nil)
                }
            } catch {
                let message = error.localizedDescription
                await MainActor.run {
                    if executable.resolvingSymlinksInPath().path == UiRuntime.evolvedExecutable(in: dataDir).resolvingSymlinksInPath().path {
                        appDelegate.runningStamp = UiRuntime.binaryStamp(at: executable)
                    }
                    appDelegate.isRelaunching = false
                    client.restart()
                    client.errorBanner = "热重启前端失败，已保留当前界面：\(message)"
                }
            }
        }
    }
}
