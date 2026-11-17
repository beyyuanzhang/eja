import Foundation

func clockTime(_ seconds: Int) -> String {
    let date = Date(timeIntervalSince1970: TimeInterval(seconds))
    let formatter = DateFormatter()
    formatter.dateFormat = "HH:mm"
    return formatter.string(from: date)
}

func dateTimeText(_ seconds: Int) -> String {
    if seconds == 0 { return "—" }
    let date = Date(timeIntervalSince1970: TimeInterval(seconds))
    let formatter = DateFormatter()
    formatter.dateFormat = "M月d日 HH:mm"
    return formatter.string(from: date)
}

func relativeText(_ seconds: Int) -> String {
    if seconds == 0 { return "" }
    let diff = max(0, Int(Date().timeIntervalSince1970) - seconds)
    if diff < 45 { return "刚刚" }
    if diff < 3600 { return "\(max(1, diff / 60)) 分钟前" }
    if diff < 86_400 { return "\(diff / 3600) 小时前" }
    let date = Date(timeIntervalSince1970: TimeInterval(seconds))
    let calendar = Calendar.current
    if calendar.isDateInYesterday(date) { return "昨天 \(clockTime(seconds))" }
    let formatter = DateFormatter()
    formatter.dateFormat = calendar.isDate(date, equalTo: Date(), toGranularity: .year) ? "M月d日" : "yyyy年M月d日"
    return formatter.string(from: date)
}

func durationText(_ seconds: Int) -> String {
    let value = max(0, seconds)
    if value < 60 { return "\(value)s" }
    if value < 3600 { return "\(value / 60):\(String(format: "%02d", value % 60))" }
    if value < 86_400 {
        let minutes = (value % 3600) / 60
        return "\(value / 3600)h\(minutes > 0 ? " \(minutes)m" : "")"
    }
    let hours = (value % 86_400) / 3600
    return "\(value / 86_400)d\(hours > 0 ? " \(hours)h" : "")"
}

func taskStatusLabel(_ status: String) -> String {
    switch status {
    case "open": return "未开始"
    case "running": return "运行中"
    case "waiting": return "等待"
    case "done": return "完成"
    case "failed": return "失败"
    default: return status
    }
}

func clippedText(_ value: String, limit: Int = 130) -> String {
    let collapsed = value.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    if collapsed.count <= limit { return collapsed }
    return String(collapsed.prefix(limit)) + "…"
}

func friendlyError(_ message: String) -> String {
    let lowered = message.lowercased()
    if lowered.contains("model is not configured") {
        return "还没接上模型。先在设置里选好 Provider 和模型。"
    }
    if lowered.contains("a turn is already running") { return "它还在处理上一件事。" }
    if lowered.contains("needs text, an approval, a denial or a situation") {
        return "还没有写下要交代的事。"
    }
    if lowered.contains("text is longer") || lowered.contains("request body too large") {
        return "这次递过去的内容太长了。"
    }
    if lowered.contains("stopped by sigint") { return "这一轮被停下了。" }
    return message
}
