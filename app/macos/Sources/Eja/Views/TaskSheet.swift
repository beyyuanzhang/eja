import SwiftUI

struct TaskSheet: View {
    let task: TaskItem
    let model: AppModel
    @Environment(\.dismiss) private var dismiss

    private var isGroup: Bool { task.kind == "group" }

    private var children: [TaskItem] {
        task.children.compactMap { childId in
            model.state?.tasks.first { $0.id == childId }
        }
    }

    private var milestones: [(label: String, at: Int)] {
        [
            ("开始", task.createdAt),
            ("交给 AJA", task.assignedAt ?? 0),
            ("回报", task.reportedAt ?? 0),
            ("判断", task.closedAt ?? 0),
        ].filter { $0.1 > 0 }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    milestoneRow
                    if !isGroup {
                        section("交代") {
                            Text(task.request.isEmpty ? "没有另写委托。" : task.request)
                                .font(.system(size: 12.5)).lineSpacing(4)
                                .foregroundStyle(Theme.inkSoft)
                                .textSelection(.enabled)
                        }
                        section("完成条件") {
                            Text(task.acceptance.isEmpty ? "没有单独写验收条件。" : task.acceptance)
                                .font(.system(size: 12.5)).lineSpacing(4)
                                .foregroundStyle(Theme.inkSoft)
                                .textSelection(.enabled)
                        }
                        voices
                        if let reconcile = task.run.reconcile, !reconcile.isNull {
                            section("世界") { jsonBlock(reconcile.prettyText) }
                        }
                    } else {
                        section("组内节点", note: "组只聚合，不执行") {
                            if children.isEmpty {
                                Text("组里还没有节点。")
                                    .font(.system(size: 12)).foregroundStyle(Theme.muted)
                            } else {
                                VStack(spacing: 8) {
                                    ForEach(children) { child in
                                        Button {
                                            model.openTask(child.id)
                                        } label: {
                                            HStack(spacing: 10) {
                                                Text(taskStatusLabel(child.status))
                                                    .font(.system(size: 10))
                                                    .foregroundStyle(Theme.muted)
                                                    .padding(.horizontal, 8)
                                                    .padding(.vertical, 3)
                                                    .background(Theme.surface2, in: Capsule())
                                                Text(child.title)
                                                    .font(.system(size: 12.5))
                                                    .foregroundStyle(Theme.inkSoft)
                                                    .lineLimit(1)
                                                Spacer()
                                                Image(systemName: "chevron.right")
                                                    .font(.system(size: 10))
                                                    .foregroundStyle(Theme.faint)
                                            }
                                            .card(padding: 10)
                                        }
                                        .buttonStyle(.plain)
                                    }
                                }
                            }
                        }
                        if let close = task.close {
                            section("eja 判") {
                                Text(close.note ?? (close.accepted ? "完成" : "失败"))
                                    .font(.system(size: 12.5)).lineSpacing(4)
                                    .foregroundStyle(Theme.inkSoft)
                            }
                        }
                    }
                    eventChips
                }
                .padding(22)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .scrollIndicators(.never)
            footer
        }
        .frame(width: 540, height: 640)
        .background(Theme.paper)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 10) {
                Text("\(isGroup ? "GROUP" : "AJA") · #\(task.id)")
                    .font(.system(size: 10, weight: .semibold))
                    .tracking(1.2)
                    .foregroundStyle(Theme.faint)
                if !task.origin.isEmpty {
                    Text("来自 · \(task.origin)")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.faint)
                        .lineLimit(1)
                }
                Spacer()
                Button {
                    dismiss()
                } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.muted)
                        .frame(width: 26, height: 26)
                        .background(Theme.surface2, in: Circle())
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.cancelAction)
            }
            if !task.workspace.isEmpty {
                Label {
                    Text(task.workspace).font(Theme.mono(10.5))
                } icon: {
                    Image(systemName: "folder").font(.system(size: 10))
                }
                .foregroundStyle(Theme.muted)
            }
            Text(task.title)
                .font(Theme.serif(22, weight: .medium))
                .foregroundStyle(Theme.ink)
                .textSelection(.enabled)
            HStack(spacing: 10) {
                Text(taskStatusLabel(task.status))
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(statusColor)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 4)
                    .background(statusColor.opacity(0.1), in: Capsule())
                Text(relativeText(task.createdAt))
                    .font(.system(size: 10.5))
                    .foregroundStyle(Theme.muted)
                if let closedAt = task.closedAt {
                    Text(durationText(closedAt - task.createdAt))
                        .font(.system(size: 10.5))
                        .foregroundStyle(Theme.muted)
                }
            }
        }
        .padding(22)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surface)
        .overlay(alignment: .bottom) { Divider().overlay(Theme.line) }
    }

    private var statusColor: Color {
        switch task.status {
        case "failed": return Theme.danger
        case "running": return Theme.accent
        case "waiting": return Theme.amber
        case "done": return Theme.inkSoft
        default: return Theme.muted
        }
    }

    private var milestoneRow: some View {
        HStack(spacing: 0) {
            ForEach(Array(milestones.enumerated()), id: \.offset) { index, milestone in
                VStack(alignment: .leading, spacing: 3) {
                    Text(milestone.label)
                        .font(.system(size: 10.5, weight: index == milestones.count - 1 ? .semibold : .regular))
                        .foregroundStyle(index == milestones.count - 1 ? Theme.inkSoft : Theme.muted)
                    Text(dateTimeText(milestone.at))
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.faint)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .card(padding: 12)
    }

    private var voices: some View {
        section("三种声音", note: "不是把过程混成一句结论") {
            VStack(spacing: 10) {
                voiceCard("它说", badge: nil) {
                    Text(task.run.digest ?? "还没有回报。")
                        .font(.system(size: 12)).lineSpacing(4)
                        .foregroundStyle(Theme.inkSoft)
                        .textSelection(.enabled)
                }
                voiceCard("宿主看见", badge: task.run.evidenceId != nil ? "已核验" : "未核验") {
                    if let evidence = task.run.evidence, !evidence.isNull {
                        VStack(alignment: .leading, spacing: 8) {
                            DisclosureGroup("验收契约") {
                                jsonBlock(evidence.prettyText)
                            }
                            .font(.system(size: 11.5))
                            .foregroundStyle(Theme.muted)
                            if task.run.evidenceId != nil, let result = task.run.evidenceResult, !result.isNull {
                                Text("实际观察")
                                    .font(.system(size: 10.5, weight: .medium))
                                    .foregroundStyle(Theme.muted)
                                jsonBlock(result.prettyText)
                            } else {
                                Text("宿主还没有看到契约要求的证据。")
                                    .font(.system(size: 11.5))
                                    .foregroundStyle(Theme.faint)
                            }
                        }
                    } else {
                        Text("这件工作没有要求宿主另外核验。")
                            .font(.system(size: 11.5))
                            .foregroundStyle(Theme.faint)
                    }
                }
                voiceCard("eja 判", badge: task.close.map { $0.accepted ? "完成" : "失败" } ?? "还没判") {
                    Text(task.close?.note ?? (task.close != nil ? "没有另写理由。" : "等前面的声音都到齐。"))
                        .font(.system(size: 12)).lineSpacing(4)
                        .foregroundStyle(Theme.inkSoft)
                        .textSelection(.enabled)
                }
            }
        }
    }

    private func voiceCard<Content: View>(_ title: String, badge: String?, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(title)
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
                Spacer()
                if let badge {
                    Text(badge)
                        .font(.system(size: 10))
                        .foregroundStyle(badge == "失败" ? Theme.danger : Theme.muted)
                }
            }
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .card(padding: 12)
    }

    private var eventChips: some View {
        section("事件") {
            let columns = [GridItem(.adaptive(minimum: 52), spacing: 6)]
            LazyVGrid(columns: columns, alignment: .leading, spacing: 6) {
                ForEach(Array(task.eventIds.prefix(12)), id: \.self) { eventId in
                    Button {
                        Task { await model.openRaw("log:event:\(eventId)") }
                    } label: {
                        Text("#\(eventId)")
                            .font(Theme.mono(10))
                            .foregroundStyle(Theme.muted)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 4)
                            .background(Theme.surface2, in: Capsule())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    private var footer: some View {
        HStack {
            Button {
                Task { await model.openRaw("log:task:\(task.id)") }
            } label: {
                Label("原始记录", systemImage: "curlybraces")
                    .font(.system(size: 11.5))
                    .foregroundStyle(Theme.muted)
            }
            .buttonStyle(.plain)
            Spacer()
            if task.run.evidenceId != nil {
                Button {
                    Task { await model.openRaw("log:event:\(task.run.evidenceId!)") }
                } label: {
                    Text("核验事件")
                        .font(.system(size: 11.5))
                        .foregroundStyle(Theme.muted)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 13)
        .background(Theme.surface)
        .overlay(alignment: .top) { Divider().overlay(Theme.line) }
    }

    private func section<Content: View>(_ title: String, note: String? = nil, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 8) {
                Text(title)
                    .font(.system(size: 11, weight: .semibold))
                    .tracking(0.8)
                    .foregroundStyle(Theme.muted)
                if let note {
                    Text(note)
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.faint)
                }
            }
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func jsonBlock(_ text: String) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            Text(text)
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.inkSoft)
                .textSelection(.enabled)
                .padding(10)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surface2, in: RoundedRectangle(cornerRadius: 8))
    }
}
