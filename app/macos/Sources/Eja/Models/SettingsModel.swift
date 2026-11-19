import Foundation
import Observation

struct WorkspaceDraft: Identifiable {
    let id = UUID()
    var name: String
    var path: String
}

@MainActor
@Observable
final class SettingsModel {
    var loading = false
    var loadError = ""
    var savedFlash = false
    var saving = false
    var dirty = false
    var hydrated = false
    var providers: [ProviderDraft] = []
    var roles: RoleBindings = .empty
    var connectors: [ConnectorDraft] = []
    var capabilities: [CapabilityDraft] = []
    var limits: Limits = .fallback
    var workspaces: [WorkspaceDraft] = []
    var view: SettingsView?

    private var flashTask: Task<Void, Never>?
    private var service = ""
    private var serviceGeneration = 0
    private var activeLoads = 0
    private var loadingService: String?

    func load(client: APIClient, service: String, force: Bool) async {
        if service != self.service {
            switchService(to: service)
        }
        if hydrated && !force { return }
        if loadingService == service { return }
        loadingService = service
        activeLoads += 1
        loading = true
        loadError = ""
        let generation = serviceGeneration
        defer {
            activeLoads -= 1
            if loadingService == service {
                loadingService = nil
            }
            loading = activeLoads > 0
        }
        do {
            let fetched = try await client.fetchSettings()
            guard generation == serviceGeneration, service == self.service else { return }
            hydrate(from: fetched)
        } catch {
            guard generation == serviceGeneration, service == self.service else { return }
            loadError = friendlyError(error.localizedDescription)
        }
    }

    func save(client: APIClient, service: String) async -> Bool {
        guard hydrated, service == self.service, !saving else { return false }
        let generation = serviceGeneration
        saving = true
        defer {
            if generation == serviceGeneration {
                saving = false
            }
        }
        let body = SettingsSaveBody(
            providers: providers,
            roles: roles,
            capabilities: capabilityMap(),
            limits: limits,
            workspaces: workspaceMap()
        )
        do {
            let saved = try await client.saveSettings(body)
            guard generation == serviceGeneration, service == self.service else { return false }
            reconcile(with: saved)
            dirty = false
            flashSaved()
            return true
        } catch {
            return false
        }
    }

    /// A settings model belongs to one service. Dropping the old drafts before a
    /// load prevents an address change from turning A's providers or workspace
    /// paths into a save against B.
    private func switchService(to service: String) {
        serviceGeneration += 1
        self.service = service
        hydrated = false
        dirty = false
        savedFlash = false
        flashTask?.cancel()
        saving = false
        providers = []
        roles = .empty
        connectors = []
        capabilities = []
        limits = .fallback
        workspaces = []
        view = nil
        loadError = ""
    }

    func markDirty() {
        dirty = true
        savedFlash = false
        flashTask?.cancel()
    }

    func addWorkspace() {
        workspaces.append(WorkspaceDraft(name: "", path: ""))
        markDirty()
    }

    func removeWorkspace(id: UUID) {
        workspaces.removeAll { $0.id == id }
        markDirty()
    }

    func addProvider() {
        providers.append(ProviderDraft())
        markDirty()
    }

    func removeProvider(id: String) {
        providers.removeAll { $0.id == id }
        if roles.eja.providerId == id { roles.eja = .empty }
        if roles.aja.providerId == id { roles.aja = .empty }
        markDirty()
    }

    func addConnector() {
        connectors.append(ConnectorDraft())
        markDirty()
    }

    func removeConnector(id: UUID) {
        connectors.removeAll { $0.id == id }
        markDirty()
    }

    private func hydrate(from view: SettingsView) {
        self.view = view
        providers = view.providers.map(ProviderDraft.init(view:))
        roles = view.roles
        capabilities = view.capabilities.map(CapabilityDraft.init(view:))
        connectors = (mcpConnectors(from: view.capabilities) ?? []).map(ConnectorDraft.init(view:))
        limits = view.limits
        workspaces = view.workspaces.keys.sorted().map { name in
            WorkspaceDraft(name: name, path: view.workspaces[name] ?? "")
        }
        dirty = false
        hydrated = true
    }

    private func workspaceMap() -> [String: String] {
        var output: [String: String] = [:]
        for row in workspaces {
            let name = row.name.trimmingCharacters(in: .whitespacesAndNewlines)
            let path = row.path.trimmingCharacters(in: .whitespacesAndNewlines)
            if name.isEmpty && path.isEmpty { continue }
            output[name] = path
        }
        return output
    }

    private func capabilityMap() -> [String: CapabilitySaveBody] {
        var output: [String: CapabilitySaveBody] = [:]
        for capability in capabilities {
            let config: JSONValue
            if capability.id == "mcp" {
                config = jsonValue(ConnectorSaveBody(servers: connectors))
            } else {
                config = capability.config
            }
            output[capability.id] = CapabilitySaveBody(enabled: capability.enabled, config: config)
        }
        return output
    }

    private func reconcile(with saved: SettingsView) {
        view = saved
        capabilities = saved.capabilities.map(CapabilityDraft.init(view:))
        limits = saved.limits
        let returnedProviders = Dictionary(uniqueKeysWithValues: saved.providers.map { ($0.id, $0) })
        for index in providers.indices {
            guard let returned = returnedProviders[providers[index].id] else { continue }
            providers[index].hasApiKey = returned.hasApiKey
            providers[index].local = returned.local
            if providers[index].clearApiKey || !providers[index].apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                providers[index].apiKey = ""
                providers[index].clearApiKey = false
            }
            providers[index].headers = syncRows(providers[index].headers, with: returned.headers)
        }
        let returnedConnectors = Dictionary(
            uniqueKeysWithValues: (mcpConnectors(from: saved.capabilities) ?? []).map { ($0.name, $0) }
        )
        for index in connectors.indices {
            let name = connectors[index].name.trimmingCharacters(in: .whitespacesAndNewlines)
            guard let returned = returnedConnectors[name] else { continue }
            connectors[index].env = syncRows(connectors[index].env, with: returned.env)
            connectors[index].headers = syncRows(connectors[index].headers, with: returned.headers)
        }
    }

    private func mcpConnectors(from capabilities: [CapabilityView]) -> [ConnectorView]? {
        guard let capability = capabilities.first(where: { $0.id == "mcp" }) else { return [] }
        guard let data = try? JSONEncoder().encode(capability.config) else { return nil }
        return try? JSONDecoder().decode(ConnectorsView.self, from: data).servers
    }

    private func jsonValue<T: Encodable>(_ value: T) -> JSONValue {
        guard let data = try? JSONEncoder().encode(value),
              let decoded = try? JSONDecoder().decode(JSONValue.self, from: data) else {
            return .object([:])
        }
        return decoded
    }

    private func syncRows(_ rows: [KeyValueDraft], with returned: [KeyValueView]) -> [KeyValueDraft] {
        let returnedByKey = Dictionary(uniqueKeysWithValues: returned.map { ($0.key, $0) })
        return rows.map { row in
            let key = row.key.trimmingCharacters(in: .whitespacesAndNewlines)
            guard let stored = returnedByKey[key] else { return row }
            var next = row
            next.secret = stored.secret
            next.hasValue = stored.hasValue
            if stored.redacted {
                next.redacted = true
                next.value = ""
            } else if row.redacted {
                next.redacted = false
                next.value = stored.value
            }
            return next
        }
    }

    private func flashSaved() {
        flashTask?.cancel()
        savedFlash = true
        flashTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 1_800_000_000)
            guard !Task.isCancelled else { return }
            self?.savedFlash = false
        }
    }
}
