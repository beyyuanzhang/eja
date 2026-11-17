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
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 8) {
                SettingsFieldLabel(text: "连接")
                VStack(alignment: .leading, spacing: 8) {
                    TextField("http://127.0.0.1:8787", text: baseURLBinding)
                        .textFieldStyle(.plain)
                        .font(Theme.mono(11.5))
                        .foregroundStyle(Theme.ink)
                        .padding(.horizontal, 8)
                        .frame(height: 28)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
                    Text("指向本地 eja 运行时的 HTTP 地址，默认 http://127.0.0.1:8787。")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.muted)
                }
                .card()
            }
            VStack(alignment: .leading, spacing: 8) {
                SettingsFieldLabel(text: "限额")
                VStack(alignment: .leading, spacing: 10) {
                    limitRow("并行槽位", text: $slotsText, hint: "1–8") { parsed in
                        model.limits.slots = min(max(parsed, 1), 8)
                    }
                    Divider().overlay(Theme.line)
                    limitRow("回合超时", text: $turnTimeoutText, hint: "ms · 0 = 默认") { parsed in
                        model.limits.turnTimeoutMs = max(parsed, 0)
                    }
                    Divider().overlay(Theme.line)
                    limitRow("AJA 步数", text: $ajaStepsText, hint: "0 = 默认") { parsed in
                        model.limits.ajaSteps = max(parsed, 0)
                    }
                    Divider().overlay(Theme.line)
                    limitRow("AJA 超时", text: $ajaTimeoutText, hint: "ms · 0 = 默认") { parsed in
                        model.limits.ajaTimeoutMs = max(parsed, 0)
                    }
                }
                .card()
            }
            VStack(alignment: .leading, spacing: 8) {
                SettingsFieldLabel(text: "路径")
                VStack(spacing: 0) {
                    pathRow("runtime", value: model.view?.runtimePath ?? "")
                    Divider().overlay(Theme.line)
                    pathRow("state", value: model.view?.stateDir ?? "")
                    Divider().overlay(Theme.line)
                    pathRow("project", value: model.view?.projectDir ?? "")
                    if let configPath = model.view?.connectors.configPath, !configPath.isEmpty {
                        Divider().overlay(Theme.line)
                        pathRow("mcp", value: configPath)
                    }
                }
                .card(padding: 6)
                if let view = model.view {
                    if view.connectors.managed {
                        Text("连接器由应用托管，保存后写入上方 mcp 配置。")
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.muted)
                    } else {
                        Text("连接器尚未托管：保存一次设置后由应用接管。")
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.amber)
                    }
                    if let error = view.connectors.error, !error.isEmpty {
                        Text(error)
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.danger)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(10)
                            .background(Theme.dangerWash, in: RoundedRectangle(cornerRadius: 9))
                            .textSelection(.enabled)
                    }
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
        HStack(spacing: 8) {
            Text(label)
                .font(.system(size: 12))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
            Text(hint)
                .font(.system(size: 10.5))
                .foregroundStyle(Theme.faint)
            TextField("0", text: text)
                .textFieldStyle(.plain)
                .font(Theme.mono(11.5))
                .foregroundStyle(Theme.ink)
                .multilineTextAlignment(.trailing)
                .frame(width: 96)
                .padding(.horizontal, 8)
                .frame(height: 26)
                .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
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
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
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
        .buttonStyle(.plain)
        .disabled(value.isEmpty)
        .help("复制路径")
    }
}
