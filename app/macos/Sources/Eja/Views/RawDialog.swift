import AppKit
import SwiftUI

struct RawDialog: View {
    let record: RawRecord
    let onClose: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: "curlybraces")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.muted)
                Text(record.address)
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer()
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(record.content, forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.muted)
                        .frame(width: 26, height: 26)
                        .background(Theme.surface2, in: Circle())
                }
                .buttonStyle(.plain)
                .help("复制内容")
                Button(action: onClose) {
                    Image(systemName: "xmark")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.muted)
                        .frame(width: 26, height: 26)
                        .background(Theme.surface2, in: Circle())
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.cancelAction)
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 13)
            .background(Theme.surface)
            .overlay(alignment: .bottom) { Divider().overlay(Theme.line) }

            ScrollView([.vertical, .horizontal]) {
                Text(record.content)
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
                    .textSelection(.enabled)
                    .padding(16)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .scrollIndicators(.never)

            if record.truncated == true {
                Text("内容过大，已截断\(record.bytes.map { "（共 \($0) 字节）" } ?? "")。")
                    .font(.system(size: 10.5))
                    .foregroundStyle(Theme.amber)
                    .padding(.vertical, 8)
                    .frame(maxWidth: .infinity)
                    .background(Theme.surface2)
            }
        }
        .frame(width: 640, height: 560)
        .background(Theme.paper)
    }
}
