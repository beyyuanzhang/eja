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
            HStack(spacing: 5) {
                Image(systemName: "plus")
                    .font(.system(size: 10, weight: .semibold))
                Text(title)
                    .font(.system(size: 11.5))
            }
            .foregroundStyle(Theme.muted)
            .padding(.horizontal, 9)
            .padding(.vertical, 5)
            .background(Theme.surface2, in: RoundedRectangle(cornerRadius: 7))
        }
        .buttonStyle(.plain)
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
                .background(armed ? Theme.dangerWash : Color.clear, in: RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
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
            ForEach($rows) { $row in
                rowEditor($row)
            }
            SettingsAddButton(title: kind.addTitle) {
                rows.append(KeyValueDraft())
                onChanged()
            }
        }
    }

    private func rowEditor(_ row: Binding<KeyValueDraft>) -> some View {
        let rowId = row.wrappedValue.id
        let isSecret = row.wrappedValue.secret && !revealed.contains(rowId)
        return HStack(spacing: 6) {
            TextField(kind.keyPlaceholder, text: row.key)
                .textFieldStyle(.plain)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.ink)
                .padding(.horizontal, 8)
                .frame(height: 28)
                .frame(maxWidth: .infinity)
                .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
                .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
                .onChange(of: row.wrappedValue.key) { _, newKey in
                    row.wrappedValue.secret = looksSecret(newKey)
                    onChanged()
                }
            HStack(spacing: 2) {
                Group {
                    if isSecret {
                        SecureField(valuePlaceholder(for: row.wrappedValue), text: row.value)
                    } else {
                        TextField(valuePlaceholder(for: row.wrappedValue), text: row.value)
                    }
                }
                .textFieldStyle(.plain)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.ink)
                if row.wrappedValue.secret {
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
                    .buttonStyle(.plain)
                    .help(revealed.contains(rowId) ? "隐藏值" : "显示值")
                }
            }
            .padding(.leading, 8)
            .padding(.trailing, 3)
            .frame(height: 28)
            .frame(maxWidth: .infinity)
            .background(Theme.surface, in: RoundedRectangle(cornerRadius: 7))
            .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Theme.line))
            .onChange(of: row.wrappedValue.value) { _, _ in
                row.wrappedValue.redacted = false
                onChanged()
            }
            SettingsDeleteButton {
                rows.removeAll { $0.id == rowId }
                revealed.remove(rowId)
                onChanged()
            }
        }
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
