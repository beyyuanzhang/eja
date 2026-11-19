import Foundation
import Observation

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
        didSet {
            guard !loadingEncounter else { return }
            persistEncounter()
        }
    }
    var optimistic: OptimisticMessage?
    var draftSeed = ""
    var selectedTaskId: Int?
    var raw: RawRecord?
    var toastState: ToastState?
    var settingsPresented = false
    var pollTick = 0

    private var fetching = false
    private var toastGeneration = 0
    private var restoredParked = false
    private var loadingEncounter = false
    private var serviceGeneration = 0

    private static let baseURLKey = "eja.baseURL"

    init() {
        let stored = UserDefaults.standard.string(forKey: Self.baseURLKey) ?? "http://127.0.0.1:8787"
        baseURLString = stored
        let url = URL(string: stored) ?? URL(string: "http://127.0.0.1:8787")!
        client = APIClient(baseURL: url)
        restoreEncounter(for: url)
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
        let previousService = Self.serviceIdentity(client.baseURL)
        let nextService = Self.serviceIdentity(url)
        baseURLString = trimmed
        client = APIClient(baseURL: url)
        guard previousService != nextService else {
            refreshSoon()
            return
        }

        serviceGeneration += 1
        fetching = false
        state = nil
        initialLoading = true
        fatal = ""
        pending = false
        optimistic = nil
        draftSeed = ""
        selectedTaskId = nil
        raw = nil
        toastState = nil
        restoreEncounter(for: url)
        refreshSoon()
        Task { await refresh(quiet: true) }
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
        let generation = serviceGeneration
        fetching = true
        defer {
            if generation == serviceGeneration {
                fetching = false
            }
        }
        do {
            let current = state
            var next = try await client.fetchState(
                busy: busy,
                encounter: encounter,
                after: current?.lastEvent ?? 0
            )
            guard generation == serviceGeneration else { return }
            if let current, next.lastEvent >= current.lastEvent {
                let received = Set(next.events.map(\.id))
                next.events = current.events.filter { !received.contains($0.id) } + next.events
            }
            if state != next {
                state = next
            }
            if restoredParked {
                restoredParked = false
                if let current = encounter, !next.loops.contains(where: { matches($0, current) }) {
                    encounter = nil
                }
            }
            if !fatal.isEmpty {
                fatal = ""
            }
        } catch {
            guard generation == serviceGeneration else { return }
            let message = friendlyError(error.localizedDescription)
            if state == nil {
                fatal = message
            } else if !quiet {
                toast(message, isError: true)
            }
        }
        if generation == serviceGeneration, initialLoading {
            initialLoading = false
        }
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

    func choose(question: Int, option: Int, label: String) {
        guard !busy else { return }
        if encounter == nil { beginEncounter(anchor: max(0, question - 1)) }
        Task {
            await transmit(
                "",
                choiceQuestion: question,
                choiceOption: option,
                optimisticLabel: label,
                showOptimistic: true
            )
        }
    }

    private func transmit(
        _ text: String,
        approvals: [Int] = [],
        denials: [Int] = [],
        gesture: String? = nil,
        choiceQuestion: Int? = nil,
        choiceOption: Int? = nil,
        optimisticLabel: String? = nil,
        showOptimistic: Bool
    ) async {
        guard !pending, let state else { return }
        let generation = serviceGeneration
        let requestClient = client
        let before = state.lastEvent
        let focus = gesture == nil ? encounter?.taskId : nil
        pending = true
        if showOptimistic {
            optimistic = OptimisticMessage(
                text: optimisticLabel ?? text,
                at: Int(Date().timeIntervalSince1970)
            )
        }
        defer {
            if generation == serviceGeneration {
                pending = false
                optimistic = nil
            }
        }
        do {
            let result = try await requestClient.sendTurn(
                text: text,
                focus: focus,
                approvals: approvals,
                denials: denials,
                gesture: gesture,
                choiceQuestion: choiceQuestion,
                choiceOption: choiceOption
            )
            guard generation == serviceGeneration else { return }
            await refresh(quiet: true)
            guard generation == serviceGeneration else { return }
            if !result.ok {
                restoreIfUnsent(
                    text,
                    after: before,
                    restore: showOptimistic && choiceQuestion == nil
                )
                toast(result.error ?? result.stderr ?? "这一轮没有走完。", isError: true)
            }
        } catch {
            guard generation == serviceGeneration else { return }
            await refresh(quiet: true)
            guard generation == serviceGeneration else { return }
            restoreIfUnsent(
                text,
                after: before,
                restore: showOptimistic && choiceQuestion == nil
            )
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
        let generation = serviceGeneration
        let requestClient = client
        do {
            let record = try await requestClient.fetchRaw(address)
            guard generation == serviceGeneration else { return }
            raw = record
        } catch {
            guard generation == serviceGeneration else { return }
            toast(error.localizedDescription, isError: true)
        }
    }

    private func persistEncounter() {
        let key = Self.encounterKey(for: client.baseURL)
        if let encounter, let data = try? JSONEncoder().encode(encounter) {
            UserDefaults.standard.set(data, forKey: key)
        } else {
            UserDefaults.standard.removeObject(forKey: key)
        }
    }

    private func restoreEncounter(for url: URL) {
        loadingEncounter = true
        defer { loadingEncounter = false }
        if let data = UserDefaults.standard.data(forKey: Self.encounterKey(for: url)),
           let parked = try? JSONDecoder().decode(Encounter.self, from: data) {
            encounter = parked
            restoredParked = true
        } else {
            encounter = nil
            restoredParked = false
        }
    }

    private static func serviceIdentity(_ url: URL) -> String {
        var components = URLComponents()
        components.scheme = url.scheme?.lowercased()
        components.host = url.host?.lowercased()
        components.port = url.port
        return components.string ?? url.absoluteString
    }

    private static func encounterKey(for url: URL) -> String {
        "eja.encounter.\(serviceIdentity(url))"
    }

    private func matches(_ loop: LoopItem, _ encounter: Encounter) -> Bool {
        if loop.kind == "decision" {
            return loop.proposal?.id == encounter.proposalId
        }
        return loop.task?.id == encounter.taskId
    }
}
