import Foundation
import Observation
import SwiftUI

struct ToastState {
    var text: String
    var isError: Bool
}

struct OptimisticMessage {
    var text: String
    var at: Int
}

@MainActor
@Observable
final class AppModel {
    var client: APIClient
    var baseURLString: String {
        didSet { UserDefaults.standard.set(baseURLString, forKey: Self.baseURLKey) }
    }

    var state: AppState?
    var initialLoading = true
    var fatal = ""
    var pending = false
    var encounter: Encounter? {
        didSet { persistEncounter() }
    }
    var optimistic: OptimisticMessage?
    var draftSeed = ""
    var selectedTaskId: Int?
    var raw: RawRecord?
    var toastState: ToastState?
    var pollTick = 0

    private var fetching = false
    private var toastGeneration = 0
    private var restoredParked = false

    private static let baseURLKey = "eja.baseURL"
    private static let encounterKey = "eja.encounter"

    init() {
        let stored = UserDefaults.standard.string(forKey: Self.baseURLKey) ?? "http://127.0.0.1:8787"
        baseURLString = stored
        client = APIClient(baseURL: URL(string: stored) ?? URL(string: "http://127.0.0.1:8787")!)
        if let data = UserDefaults.standard.data(forKey: Self.encounterKey),
           let parked = try? JSONDecoder().decode(Encounter.self, from: data) {
            encounter = parked
            restoredParked = true
        }
    }

    var busy: Bool { pending || (state?.health.turnInFlight ?? false) }
    var pollDelay: Duration { busy ? .seconds(2) : .seconds(5) }

    var presence: ChatMessage? {
        guard let state, encounter == nil else { return nil }
        return state.presence
    }

    var loops: [LoopItem] {
        state?.loops ?? []
    }

    var actions: [LoopItem] {
        encounter == nil ? [] : (state?.loops ?? [])
    }

    var encounterMessages: [ChatMessage] {
        state?.spoken ?? []
    }

    var backgroundTasks: [TaskItem] {
        (state?.tasks ?? []).filter { $0.status == "running" && busy }
    }

    var selectedTask: TaskItem? {
        guard let selectedTaskId else { return nil }
        return state?.tasks.first { $0.id == selectedTaskId }
    }

    func updateBaseURL(_ string: String) {
        let trimmed = string.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = URL(string: trimmed), let scheme = url.scheme,
              scheme == "http" || scheme == "https", url.host != nil else {
            toast("这个地址读不出来。", isError: true)
            return
        }
        baseURLString = trimmed
        client.baseURL = url
        refreshSoon()
    }

    func refreshSoon() {
        pollTick += 1
    }

    func toast(_ message: String, isError: Bool = false) {
        toastGeneration += 1
        let generation = toastGeneration
        toastState = ToastState(text: isError ? friendlyError(message) : message, isError: isError)
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(isError ? 6.5 : 2.8))
            guard let self, self.toastGeneration == generation else { return }
            self.toastState = nil
        }
    }

    func closeToast() {
        toastGeneration += 1
        toastState = nil
    }

    func refresh(quiet: Bool = false) async {
        if fetching { return }
        fetching = true
        defer { fetching = false }
        do {
            let next = try await client.fetchState(busy: busy, encounter: encounter)
            state = next
            if restoredParked {
                restoredParked = false
                if let current = encounter, !next.loops.contains(where: { matches($0, current) }) {
                    encounter = nil
                }
            }
            fatal = ""
        } catch {
            let message = friendlyError(error.localizedDescription)
            if state == nil {
                fatal = message
            } else if !quiet {
                toast(message, isError: true)
            }
        }
        initialLoading = false
    }

    func beginEncounter(anchor: Int? = nil, taskId: Int? = nil, proposalId: Int? = nil) {
        guard let state else { return }
        encounter = Encounter(anchor: anchor ?? state.lastEvent, taskId: taskId, proposalId: proposalId)
        refreshSoon()
    }

    func openLoop(_ loop: LoopItem) {
        if loop.kind == "decision", let proposal = loop.proposal {
            beginEncounter(proposalId: proposal.id)
        } else if let task = loop.task {
            beginEncounter(taskId: task.id)
        }
    }

    func leaveEncounter() {
        guard encounter != nil, !busy else { return }
        encounter = nil
        refreshSoon()
        Task { await transmit("", gesture: "leave", showOptimistic: false) }
    }

    func send(_ text: String) async {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !pending, state != nil else { return }
        if encounter == nil { beginEncounter() }
        await transmit(trimmed, showOptimistic: true)
    }

    func arrive() async {
        if state == nil {
            await refresh(quiet: true)
        }
        guard state != nil, encounter == nil, !busy else { return }
        await transmit("", gesture: "return", showOptimistic: false)
    }

    func approve(_ proposalId: Int) {
        Task { await transmit("", approvals: [proposalId], showOptimistic: false) }
    }

    func deny(_ proposalId: Int) {
        Task { await transmit("", denials: [proposalId], showOptimistic: false) }
    }

    private func transmit(
        _ text: String,
        approvals: [Int] = [],
        denials: [Int] = [],
        gesture: String? = nil,
        showOptimistic: Bool
    ) async {
        guard !pending, let state else { return }
        let before = state.lastEvent
        let focus = gesture == nil ? encounter?.taskId : nil
        pending = true
        if showOptimistic {
            optimistic = OptimisticMessage(text: text, at: Int(Date().timeIntervalSince1970))
        }
        defer {
            pending = false
            optimistic = nil
        }
        do {
            let result = try await client.sendTurn(text: text, focus: focus, approvals: approvals, denials: denials, gesture: gesture)
            await refresh(quiet: true)
            if !result.ok {
                restoreIfUnsent(text, after: before, restore: showOptimistic)
                toast(result.error ?? result.stderr ?? "这一轮没有走完。", isError: true)
            }
        } catch {
            await refresh(quiet: true)
            restoreIfUnsent(text, after: before, restore: showOptimistic)
            toast(error.localizedDescription, isError: true)
        }
    }

    private func restoreIfUnsent(_ text: String, after: Int, restore: Bool) {
        guard restore, !text.isEmpty else { return }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let recorded = state?.events.contains { event in
            event.id > after && event.kind == "user"
                && event.text.trimmingCharacters(in: .whitespacesAndNewlines) == trimmed
        } ?? false
        if !recorded { draftSeed = text }
    }

    func openTask(_ taskId: Int) {
        selectedTaskId = taskId
    }

    func openRaw(_ address: String) async {
        do {
            raw = try await client.fetchRaw(address)
        } catch {
            toast(error.localizedDescription, isError: true)
        }
    }

    private func persistEncounter() {
        if let encounter, let data = try? JSONEncoder().encode(encounter) {
            UserDefaults.standard.set(data, forKey: Self.encounterKey)
        } else {
            UserDefaults.standard.removeObject(forKey: Self.encounterKey)
        }
    }

    private func matches(_ loop: LoopItem, _ encounter: Encounter) -> Bool {
        if loop.kind == "decision" {
            return loop.proposal?.id == encounter.proposalId
        }
        return loop.task?.id == encounter.taskId
    }
}
