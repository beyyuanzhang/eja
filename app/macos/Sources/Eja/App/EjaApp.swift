import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

@main
struct EjaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model = AppModel()

    var body: some Scene {
        Window("eja", id: "main") {
            ContentView(model: model)
                .frame(minWidth: 560, minHeight: 460)
                .preferredColorScheme(.light)
                .task {
                    await model.arrive()
                    await pollLoop()
                }
        }
        .defaultSize(width: 860, height: 700)
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(after: .newItem) {
                Button("刷新") { model.refreshSoon() }
                    .keyboardShortcut("r", modifiers: .command)
            }
            CommandGroup(replacing: .appSettings) {
                Button("设置…") { model.settingsPresented = true }
                    .keyboardShortcut(",", modifiers: .command)
            }
        }
    }

    private func pollLoop() async {
        while !Task.isCancelled {
            await model.refresh(quiet: true)
            let tick = model.pollTick
            let delay = model.pollDelay
            var elapsed = Duration.zero
            while elapsed < delay && model.pollTick == tick && !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(200))
                elapsed += .milliseconds(200)
            }
        }
    }
}
