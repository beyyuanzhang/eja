import AppKit
import SwiftUI

struct RuntimeSection: View {
    let app: AppModel
    @Bindable var model: SettingsModel
    @State private var slotsText = "1"
    @State private var turnTimeoutText = "0"
    @State private var ajaStepsText = "0"
    @State private var ajaTimeoutText = "0"

    var body: some View {
        SettingsPage {
            SettingsGroup("本机实例") {
                VStack(spacing: 0) {
                    SettingsRow("状态", note: app.supervisor.isManaged ? "由 Eja 启动" : "外部运行时") {
                        HStack(spacing: 10) {
                            Text(app.statusTitle)
                                .font(.system(size: 12, weight: .medium))
                                .foregroundStyle(Theme.ink)
                            Button("重启") { app.restartRuntime() }
                                .buttonStyle(.bordered)
                                .controlSize(.small)
                                .disabled(!app.canRestartRuntime)
                        }
                    }
                    SettingsRule()
                    SettingsRow("工作目录", note: "新建任务时的默认目录") {
                        HStack(spacing: 8) {
                            Text(app.projectPath)
                                .font(Theme.mono(10.5))
                                .foregroundStyle(Theme.muted)
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .frame(width: 250, alignment: .trailing)
                            Button("选择…") { app.chooseProjectDirectory() }
                                .buttonStyle(.bordered)
                                .controlSize(.small)
                        }
                    }
                    SettingsRule()
                    pathRow("状态", value: app.statePath)
                    SettingsRule()
                    pathRow("日志", value: app.logPath)
                    SettingsRule()
                    Text("关闭窗口不会停止实例；退出 Eja 会停止由 Eja 启动的实例。")
                        .font(.system(size: 10.5))
                        .foregroundStyle(Theme.faint)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 10)
                }
            }

            SettingsGroup("连接") {
                SettingsRow("地址") {
                    TextField("http://127.0.0.1:8787", text: baseURLBinding)
                        .textFieldStyle(.roundedBorder)
                        .font(Theme.mono(11.5))
                        .multilineTextAlignment(.leading)
                        .labelsHidden()
                        .frame(width: 340)
                }
            }

            SettingsGroup("限额") {
                VStack(spacing: 0) {
                    limitRow("并行槽位", text: $slotsText, hint: "1–8") { parsed in
                        model.limits.slots = min(max(parsed, 1), 8)
                    }
                    SettingsRule()
                    limitRow("回合超时", text: $turnTimeoutText, hint: "ms · 0 = 默认") { parsed in
                        model.limits.turnTimeoutMs = max(parsed, 0)
                    }
                    SettingsRule()
                    limitRow("AJA 步数", text: $ajaStepsText, hint: "0 = 默认") { parsed in
                        model.limits.ajaSteps = max(parsed, 0)
                    }
                    SettingsRule()
                    limitRow("AJA 超时", text: $ajaTimeoutText, hint: "ms · 0 = 默认") { parsed in
                        model.limits.ajaTimeoutMs = max(parsed, 0)
                    }
                }
            }

            SettingsGroup("路径") {
                VStack(spacing: 0) {
                    pathRow("runtime", value: model.view?.runtimePath ?? "")
                    SettingsRule()
                    pathRow("state", value: model.view?.stateDir ?? "")
                    SettingsRule()
                    pathRow("project", value: model.view?.projectDir ?? "")
                }
            }
        }
        .onAppear(perform: syncTexts)
        .onChange(of: model.hydrated) { _, _ in syncTexts() }
    }

    private var baseURLBinding: Binding<String> {
        Binding(
            get: { app.baseURLString },
            set: { app.updateBaseURL($0) }
        )
    }

    private func syncTexts() {
        slotsText = String(model.limits.slots)
        turnTimeoutText = String(model.limits.turnTimeoutMs)
        ajaStepsText = String(model.limits.ajaSteps)
        ajaTimeoutText = String(model.limits.ajaTimeoutMs)
    }

    private func limitRow(
        _ label: String,
        text: Binding<String>,
        hint: String,
        apply: @escaping (Int) -> Void
    ) -> some View {
        SettingsRow(label, note: hint) {
            TextField("0", text: text)
                .textFieldStyle(.roundedBorder)
                .font(Theme.mono(11.5))
                .multilineTextAlignment(.trailing)
                .frame(width: 96)
                .labelsHidden()
                .onChange(of: text.wrappedValue) { _, next in
                    if let parsed = Int(next.trimmingCharacters(in: .whitespaces)), parsed >= 0 {
                        apply(parsed)
                        model.markDirty()
                    }
                }
        }
    }

    private func pathRow(_ label: String, value: String) -> some View {
        HStack(spacing: 8) {
            Text(label)
                .font(Theme.mono(9.5))
                .foregroundStyle(Theme.faint)
                .frame(width: 52, alignment: .leading)
            Text(value.isEmpty ? "—" : value)
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.muted)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
            CopyPathButton(value: value)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 9)
    }
}

private struct CopyPathButton: View {
    let value: String
    @State private var copied = false

    var body: some View {
        Button {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(value, forType: .string)
            copied = true
            Task {
                try? await Task.sleep(nanoseconds: 1_400_000_000)
                copied = false
            }
        } label: {
            Image(systemName: copied ? "checkmark" : "doc.on.doc")
                .font(.system(size: 10.5))
                .foregroundStyle(copied ? Theme.accent : Theme.faint)
                .frame(width: 24, height: 24)
        }
        .buttonStyle(.borderless)
        .disabled(value.isEmpty)
        .help("复制路径")
    }
}
