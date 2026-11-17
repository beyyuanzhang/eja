import SwiftUI

struct ContentView: View {
    let model: AppModel
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            Theme.paper.ignoresSafeArea()

            VStack(spacing: 0) {
                if let journalError = model.state?.health.journalError, !journalError.isEmpty {
                    journalBanner
                }
                stage
            }

            settingsButton
                .padding(.leading, 16)
                .padding(.bottom, 15)
        }
        .overlay(alignment: .bottom) {
            if let toast = model.toastState {
                ToastView(toast: toast) { model.closeToast() }
                    .padding(.bottom, 21)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(.easeOut(duration: 0.18), value: model.toastState != nil)
        .sheet(item: taskBinding) { task in
            TaskSheet(task: task, model: model)
        }
        .sheet(item: rawBinding) { record in
            RawDialog(record: record) { model.raw = nil }
        }
        .tint(Theme.accent)
        .foregroundStyle(Theme.ink)
    }

    private var stage: some View {
        Group {
            if model.initialLoading && model.state == nil {
                loadingStage
            } else if model.state == nil {
                fatalStage
            } else if model.encounter != nil {
                EncounterView(model: model)
            } else {
                ReturnView(model: model)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var loadingStage: some View {
        VStack(spacing: 15) {
            ProgressView()
                .controlSize(.small)
            Text("正在把日志展开")
                .font(.system(size: 11))
                .foregroundStyle(Theme.muted)
        }
    }

    private var fatalStage: some View {
        VStack(spacing: 14) {
            Image(systemName: "exclamationmark.circle")
                .font(.system(size: 22, weight: .light))
                .foregroundStyle(Theme.danger)
            Text("没连上。")
                .font(Theme.serif(34, weight: .medium))
            Text(model.fatal)
                .font(.system(size: 12))
                .foregroundStyle(Theme.muted)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            Text("cargo run -- serve --dir .eja --project .")
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
                .card(padding: 12)
            HStack(spacing: 13) {
                Button("再试一次") { model.refreshSoon() }
                    .buttonStyle(PrimaryButtonStyle())
                Text("确认运行时已启动，或在设置里核对地址")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.faint)
            }
            .padding(.top, 4)
        }
        .padding(30)
    }

    private var journalBanner: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 11))
            Text("日志里有一段读不了。已经读到的事实仍然保留。")
            Spacer()
            Button("重读") { model.refreshSoon() }
                .buttonStyle(.plain)
                .underline()
        }
        .font(.system(size: 10.5))
        .foregroundStyle(Theme.danger)
        .padding(.horizontal, 20)
        .padding(.vertical, 8)
        .background(Theme.dangerWash)
    }

    private var settingsButton: some View {
        Button {
            openSettings()
        } label: {
            Image(systemName: "slider.horizontal.3")
                .font(.system(size: 13, weight: .regular))
                .foregroundStyle(Theme.muted)
                .frame(width: 34, height: 34)
                .background(Theme.surface.opacity(0.85), in: Circle())
                .overlay(Circle().strokeBorder(Theme.line))
        }
        .buttonStyle(.plain)
        .help("设置（⌘,）")
    }

    private var taskBinding: Binding<TaskItem?> {
        Binding(get: { model.selectedTask }, set: { model.selectedTaskId = $0?.id })
    }

    private var rawBinding: Binding<RawRecord?> {
        Binding(get: { model.raw }, set: { model.raw = $0 })
    }
}

extension RawRecord: Identifiable {
    var id: String { address }
}

struct PrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 12, weight: .medium))
            .foregroundStyle(Theme.surface)
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Theme.accent.opacity(configuration.isPressed ? 0.8 : 1), in: Capsule())
    }
}

struct ToastView: View {
    let toast: ToastState
    let onClose: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            if toast.isError {
                Image(systemName: "exclamationmark.circle")
                    .font(.system(size: 12))
            } else {
                Circle().fill(Theme.accent).frame(width: 6, height: 6)
            }
            Text(toast.text)
                .font(.system(size: 11))
                .lineLimit(3)
            Button(action: onClose) {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(.white.opacity(0.56))
                    .frame(width: 22, height: 22)
            }
            .buttonStyle(.plain)
        }
        .foregroundStyle(.white.opacity(0.91))
        .padding(.leading, 13)
        .padding(.trailing, 6)
        .padding(.vertical, 8)
        .background(
            (toast.isError ? Color(red: 0x40 / 255, green: 0x31 / 255, blue: 0x2C / 255) : Theme.ink)
                .opacity(0.94),
            in: Capsule()
        )
        .frame(maxWidth: 520)
        .shadow(color: Theme.ink.opacity(0.22), radius: 22, y: 10)
    }
}
