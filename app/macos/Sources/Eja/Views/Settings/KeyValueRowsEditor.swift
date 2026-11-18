import SwiftUI

struct SettingsFieldLabel: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.system(size: 10.5, weight: .semibold))
            .tracking(0.4)
            .foregroundStyle(Theme.muted)
    }
}

struct SettingsAddButton: View {
    let title: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: "plus")
                .font(.system(size: 11.5))
                .foregroundStyle(Theme.accent)
        }
        .buttonStyle(.borderless)
    }
}

struct SettingsDeleteButton: View {
    let action: () -> Void
    @State private var armed = false

    var body: some View {
        Button {
            if armed {
                action()
            } else {
                armed = true
            }
        } label: {
            Image(systemName: armed ? "trash.fill" : "trash")
                .font(.system(size: 11.5))
                .foregroundStyle(armed ? Theme.danger : Theme.faint)
                .frame(width: 24, height: 24)
        }
        .buttonStyle(.borderless)
        .help(armed ? "再点一次确认删除" : "删除")
    }
}

struct KeyValueRowsEditor: View {
    enum Kind {
        case env
        case headers

        var keyPlaceholder: String { self == .env ? "VARIABLE_NAME" : "Authorization" }
        var valuePlaceholder: String { self == .env ? "value 或 ${VAR}" : "value" }
        var addTitle: String { self == .env ? "添加环境变量" : "添加 Header" }
    }

    @Binding var rows: [KeyValueDraft]
    var kind: Kind
    let onChanged: () -> Void
    @State private var revealed: Set<UUID> = []

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(rows) { row in
                rowEditor(row)
            }
            SettingsAddButton(title: kind.addTitle) {
                rows.append(KeyValueDraft())
                onChanged()
            }
        }
    }

    private func rowEditor(_ row: KeyValueDraft) -> some View {
        let rowId = row.id
        let isSecret = row.secret && !revealed.contains(rowId)
        return HStack(spacing: 6) {
            TextField(kind.keyPlaceholder, text: textBinding(rowId, \.key))
                .textFieldStyle(.roundedBorder)
                .font(Theme.mono(11))
                .multilineTextAlignment(.leading)
                .frame(maxWidth: .infinity)
                .labelsHidden()
            HStack(spacing: 4) {
                Group {
                    if isSecret {
                        SecureField(valuePlaceholder(for: row), text: textBinding(rowId, \.value))
                    } else {
                        TextField(valuePlaceholder(for: row), text: textBinding(rowId, \.value))
                    }
                }
                .textFieldStyle(.roundedBorder)
                .font(Theme.mono(11))
                .multilineTextAlignment(.leading)
                .labelsHidden()
                if row.secret {
                    Button {
                        if revealed.contains(rowId) {
                            revealed.remove(rowId)
                        } else {
                            revealed.insert(rowId)
                        }
                    } label: {
                        Image(systemName: revealed.contains(rowId) ? "eye.slash" : "eye")
                            .font(.system(size: 10.5))
                            .foregroundStyle(Theme.faint)
                            .frame(width: 22, height: 22)
                    }
                    .buttonStyle(.borderless)
                    .help(revealed.contains(rowId) ? "隐藏值" : "显示值")
                }
            }
            .frame(maxWidth: .infinity)
            SettingsDeleteButton {
                rows.removeAll { $0.id == rowId }
                revealed.remove(rowId)
                onChanged()
            }
        }
    }

    private func textBinding(
        _ id: UUID,
        _ keyPath: WritableKeyPath<KeyValueDraft, String>
    ) -> Binding<String> {
        Binding(
            get: { rows.first(where: { $0.id == id })?[keyPath: keyPath] ?? "" },
            set: { next in
                guard let index = rows.firstIndex(where: { $0.id == id }) else { return }
                rows[index][keyPath: keyPath] = next
                if keyPath == \.key {
                    rows[index].secret = looksSecret(next)
                } else {
                    rows[index].redacted = false
                }
                onChanged()
            }
        )
    }

    private func valuePlaceholder(for row: KeyValueDraft) -> String {
        row.redacted && row.value.isEmpty ? "已保存（保持不变）" : kind.valuePlaceholder
    }
}

func looksSecret(_ key: String) -> Bool {
    let lowered = key.lowercased()
    return ["secret", "token", "key", "pass", "auth", "credential", "cookie", "session"]
        .contains { lowered.contains($0) }
}
