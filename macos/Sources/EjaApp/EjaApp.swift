import AppKit
import EjaClient
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    var onTerminate: (() -> Void)?

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationWillTerminate(_ notification: Notification) {
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
                }
        }
        .defaultSize(width: 780, height: 600)

        Settings {
            SettingsView(client: client)
        }

        MenuBarExtra {
            Text("EJA · \(client.statusDetail)")
            if let nextWake = client.wakes.first {
                Text("下次唤醒：\(nextWake.fireAtDisplay)")
            }
            Divider()
            Button("打开 EJA") {
                NSApp.activate(ignoringOtherApps: true)
                openWindow(id: "main")
            }
            .keyboardShortcut("o")
            Button("立即整理记忆") {
                client.consolidateNow()
            }
            .disabled(!client.isConfigured)
            Divider()
            Button("退出 EJA") {
                client.shutdown()
                NSApp.terminate(nil)
            }
            .keyboardShortcut("q")
        } label: {
            Image(systemName: client.status == "thinking" ? "sparkles" : (client.status == "consolidating" ? "moon.stars" : "circle.inset.filled"))
                .accessibilityLabel("EJA")
        }
    }
}
