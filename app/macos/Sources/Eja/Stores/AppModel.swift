import AppKit
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
    let supervisor: RuntimeSupervisor
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

    private var pollTask: Task<Void, Never>?
    private var didArrive = false
    private var fetching = false
    private var toastGeneration = 0
    private var restoredParked = false
    private var loadingEncounter = false
    private var serviceGeneration = 0

    private static let baseURLKey = "eja.baseURL"

    init() {
        let stored = ProcessInfo.processInfo.environment["EJA_BASE_URL"]
            ?? UserDefaults.standard.string(forKey: Self.baseURLKey)
            ?? "http://127.0.0.1:8787"
        baseURLString = stored
        let url = URL(string: stored) ?? URL(string: "http://127.0.0.1:8787")!
        client = APIClient(baseURL: url)
        supervisor = RuntimeSupervisor(
            baseURL: url,
            stateDirectory: AppPaths.stateDirectory,
            projectDirectory: AppPaths.projectDirectory,
            logFile: AppPaths.logFile
        )
        restoreEncounter(for: url)
    }

    var busy: Bool { pending || (state?.health.turnInFlight ?? false) }
    var pollDelay: Duration { busy ? .seconds(2) : .seconds(5) }
    var serviceKey: String { Self.serviceIdentity(client.baseURL) }
    var projectPath: String { supervisor.projectDirectory.path }
    var statePath: String { supervisor.stateDirectory.path }
    var logPath: String { supervisor.logFile.path }
    var canRestartRuntime: Bool {
        if supervisor.isManaged { return true }
        if case .failed = supervisor.phase { return supervisor.isLocal }
        return false
    }

    var statusTitle: String {
        guard state != nil else {
            switch supervisor.phase {
            case .starting:
                return "正在启动"
            case .failed:
                return "实例未启动"
            case .idle:
                return "正在连接"
            case .running:
                return "正在连接"
            }
        }
        if busy { return "运行中" }
        if !loops.isEmpty { return "\(loops.count) 项待处理" }
        return "空闲"
    }

    var statusDetail: String {
        guard state != nil else { return supervisor.statusDetail }
        if busy { return "EJA 正在执行这一轮。" }
        if !loops.isEmpty { return "有决定或任务在等你。" }
        return supervisor.isManaged
            ? "由 Eja 启动 · \(baseURLString)"
            : "外部实例 · \(baseURLString)"
    }

    var menuIcon: String {
        guard state != nil else {
            switch supervisor.phase {
            case .starting:
                return "arrow.triangle.2.circlepath"
            case .failed:
                return "exclamationmark.triangle"
            case .idle, .running:
                return "circle.dashed"
            }
        }
        if busy { return "circle.dotted" }
        if !loops.isEmpty { return "bell.badge" }
        return "circle"
    }

    var menuBadge: String? {
        guard state != nil, !busy, !loops.isEmpty else { return nil }
        return String(loops.count)
    }

    func start() {
        guard pollTask == nil else { return }
        pollTask = Task { [weak self] in
            guard let self else { return }
            await self.supervisor.ensureRunning()
            await self.refresh(quiet: true)
            self.arriveIfNeeded()
            while !Task.isCancelled {
                await self.refresh(quiet: true)
                self.arriveIfNeeded()
                let tick = self.pollTick
                let delay = self.pollDelay
                var elapsed = Duration.zero
                while elapsed < delay && self.pollTick == tick && !Task.isCancelled {
                    try? await Task.sleep(for: .milliseconds(200))
                    elapsed += .milliseconds(200)
                }
            }
        }
    }

    func stopRuntime() {
        pollTask?.cancel()
        pollTask = nil
        supervisor.stop()
    }

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
            Task {
                await supervisor.reconfigure(baseURL: url)
                await refresh(quiet: true)
            }
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
        didArrive = false
        restoreEncounter(for: url)
        refreshSoon()
        Task {
            await supervisor.reconfigure(baseURL: url)
            await refresh(quiet: true)
        }
    }

    func retryRuntime() {
        Task {
            toast("正在启动 Eja 实例…")
            await supervisor.ensureRunning()
            await refresh(quiet: false)
            if case .failed(let message) = supervisor.phase {
                toast(message, isError: true)
            } else if state != nil {
                toast("Eja 已连上。")
            }
        }
    }

    func restartRuntime() {
        Task {
            guard canRestartRuntime else {
                toast("当前连接的是外部实例，Eja 不会结束它。", isError: true)
                return
            }
            toast("正在重启 Eja 实例…")
            await supervisor.restart()
            didArrive = false
            await refresh(quiet: false)
            if case .failed(let message) = supervisor.phase {
                toast(message, isError: true)
            } else if state != nil {
                toast("Eja 已重启。")
            }
        }
    }

    func chooseProjectDirectory() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "选择"
        panel.message = "选择 EJA 工作的默认目录"
        panel.directoryURL = supervisor.projectDirectory
        guard panel.runModal() == .OK, let url = panel.url else { return }
        AppPaths.setProjectDirectory(url)
        let wasManaged = supervisor.isManaged
        Task {
            toast("工作目录已更新。")
            await supervisor.updateProjectDirectory(url)
            didArrive = false
            await refresh(quiet: true)
            if !wasManaged {
                toast("新目录会在下次启动实例时生效。")
            }
        }
    }

    func openRuntimeLog() {
        supervisor.openLog()
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

    private func arriveIfNeeded() {
        guard !didArrive, state != nil, encounter == nil, !busy else { return }
        didArrive = true
        Task { await arrive() }
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
        let focus = gesture == nil && choiceQuestion == nil ? encounter?.taskId : nil
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
