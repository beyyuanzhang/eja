import Foundation

struct AppState: Codable, Sendable, Equatable {
    var events: [JournalEvent]
    var tasks: [TaskItem]
    var loops: [LoopItem]
    var presence: ChatMessage?
    var spoken: [ChatMessage]
    var lastEvent: Int
    var health: Health
}

struct JournalEvent: Codable, Sendable, Identifiable, Equatable {
    var id: Int
    var at: Int
    var turn: Int
    var scope: String
    var kind: String
    var text: String
    var closes: Int?
    var data: JSONValue?
}

struct ChatMessage: Codable, Sendable, Identifiable, Equatable {
    var id: Int
    var role: String
    var text: String
    var at: Int
    var turn: Int
    var data: JSONValue?

    var choices: [ChoiceView] {
        guard role == "eja",
              case .object(let data)? = data,
              case .array(let items)? = data["choices"] else {
            return []
        }
        return items.enumerated().compactMap { index, item in
            guard case .object(let choice) = item,
                  let label = choice["label"]?.stringValue,
                  !label.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                return nil
            }
            return ChoiceView(index: index, label: label)
        }
    }
}

struct ChoiceView: Identifiable, Equatable {
    var index: Int
    var label: String

    var id: Int { index }
}

struct TaskItem: Codable, Sendable, Identifiable, Equatable {
    var id: Int
    var kind: String
    var title: String
    var request: String
    var origin: String
    var workspace: String
    var acceptance: String
    var status: String
    var createdAt: Int
    var assignedAt: Int?
    var reportedAt: Int?
    var closedAt: Int?
    var close: TaskClose?
    var run: TaskRun
    var eventIds: [Int]
    var children: [Int]

    enum CodingKeys: String, CodingKey {
        case id, kind, title, request, origin, workspace, acceptance, status, close, run, children
        case createdAt = "created_at"
        case assignedAt = "assigned_at"
        case reportedAt = "reported_at"
        case closedAt = "closed_at"
        case eventIds = "event_ids"
    }
}

struct TaskClose: Codable, Sendable, Equatable {
    var accepted: Bool
    var note: String?
}

struct TaskRun: Codable, Sendable, Equatable {
    var digest: String?
    var evidence: JSONValue?
    var evidenceId: Int?
    var evidenceResult: JSONValue?
    var witnessAt: Int?
    var reconcile: JSONValue?

    enum CodingKeys: String, CodingKey {
        case digest, evidence, reconcile
        case evidenceId = "evidence_id"
        case evidenceResult = "evidence_result"
        case witnessAt = "witness_at"
    }
}

struct Proposal: Codable, Sendable, Identifiable, Equatable {
    var id: Int
    var kind: String
    var targetId: Int?
    var text: String
    var at: Int
    var turn: Int

    enum CodingKeys: String, CodingKey {
        case id, kind, text, at, turn
        case targetId = "target_id"
    }
}

struct Health: Codable, Sendable, Equatable {
    var model: Bool
    var modelName: String
    var state: Bool
    var mcp: Bool
    var journal: String
    var journalError: String?
    var torn: Bool
    var turnInFlight: Bool
}

struct LoopItem: Codable, Sendable, Identifiable, Equatable {
    var key: String
    var kind: String
    var eventId: Int
    var at: Int
    var title: String
    var proposal: Proposal?
    var task: TaskItem?

    var id: String { key }
}

struct Encounter: Codable, Sendable, Equatable {
    var anchor: Int
    var taskId: Int?
    var proposalId: Int?
}

struct TurnResult: Codable, Sendable {
    var ok: Bool
    var error: String?
    var stderr: String?
    var stdout: String?
}

struct RawRecord: Codable, Sendable {
    var address: String
    var kind: String
    var content: String
    var bytes: Int?
    var truncated: Bool?
}
