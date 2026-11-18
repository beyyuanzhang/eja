import Foundation

struct APIError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

@MainActor
final class APIClient {
    var baseURL: URL
    private let session: URLSession

    init(baseURL: URL) {
        self.baseURL = baseURL
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 7_200
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: configuration)
    }

    func fetchState(busy: Bool, encounter: Encounter?, after: Int) async throws -> AppState {
        var query = [URLQueryItem(name: "busy", value: busy ? "1" : "0")]
        query.append(URLQueryItem(name: "after", value: String(after)))
        if let encounter {
            query.append(URLQueryItem(name: "anchor", value: String(encounter.anchor)))
            if let taskId = encounter.taskId {
                query.append(URLQueryItem(name: "taskId", value: String(taskId)))
            }
            if let proposalId = encounter.proposalId {
                query.append(URLQueryItem(name: "proposalId", value: String(proposalId)))
            }
        }
        return try await get("/api/state", query: query)
    }

    func fetchSettings() async throws -> SettingsView {
        try await get("/api/settings")
    }

    func saveSettings(_ body: SettingsSaveBody) async throws -> SettingsView {
        try await post("/api/settings", body: body)
    }

    func discover(_ body: DiscoverBody) async throws -> DiscoverResult {
        try await post("/api/provider/discover", body: body)
    }

    func testConnector(_ draft: ConnectorDraft) async throws -> ConnectorTestResult {
        struct Envelope: Encodable { let connector: ConnectorDraft }
        return try await post("/api/capabilities/mcp/probe", body: Envelope(connector: draft))
    }

    func sendTurn(
        text: String,
        focus: Int?,
        approvals: [Int],
        denials: [Int],
        gesture: String?,
        choiceQuestion: Int? = nil,
        choiceOption: Int? = nil
    ) async throws -> TurnResult {
        struct Selection: Encodable {
            let question: Int
            let option: Int
        }
        struct Body: Encodable {
            let text: String
            let selection: Selection?
            let focus: Int?
            let approvals: [Int]
            let denials: [Int]
            let gesture: String?
        }
        let selection: Selection?
        if let choiceQuestion, let choiceOption {
            selection = Selection(question: choiceQuestion, option: choiceOption)
        } else {
            selection = nil
        }
        return try await post("/api/turn", body: Body(
            text: text,
            selection: selection,
            focus: focus,
            approvals: approvals,
            denials: denials,
            gesture: gesture
        ))
    }

    func fetchRaw(_ address: String) async throws -> RawRecord {
        try await get("/api/raw", query: [URLQueryItem(name: "address", value: address)])
    }

    private func get<Response: Decodable>(_ path: String, query: [URLQueryItem] = []) async throws -> Response {
        var request = URLRequest(url: url(path, query: query))
        request.httpMethod = "GET"
        return try await perform(request)
    }

    private func post<Body: Encodable, Response: Decodable>(_ path: String, body: Body) async throws -> Response {
        var request = URLRequest(url: url(path, query: []))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let encoder = JSONEncoder()
        request.httpBody = try encoder.encode(body)
        return try await perform(request)
    }

    private func url(_ path: String, query: [URLQueryItem]) -> URL {
        var components = URLComponents()
        components.scheme = baseURL.scheme
        components.host = baseURL.host
        components.port = baseURL.port
        components.path = path
        if !query.isEmpty { components.queryItems = query }
        return components.url ?? baseURL.appendingPathComponent(path)
    }

    private func perform<Response: Decodable>(_ request: URLRequest) async throws -> Response {
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch {
            throw APIError(message: "连不上运行时：\(error.localizedDescription)")
        }
        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw APIError(message: Self.errorMessage(from: data) ?? "HTTP \(http.statusCode)")
        }
        do {
            return try JSONDecoder().decode(Response.self, from: data)
        } catch {
            throw APIError(message: "响应读不出来：\(error.localizedDescription)")
        }
    }

    private static func errorMessage(from data: Data) -> String? {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        if let error = object["error"] as? String, !error.isEmpty { return error }
        if let error = object["error"] as? [String: Any], let message = error["message"] as? String { return message }
        return nil
    }
}
