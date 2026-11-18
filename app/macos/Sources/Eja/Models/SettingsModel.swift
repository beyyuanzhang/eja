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
    var capabilities: CapabilitySettings = .defaults
    var limits: Limits = .fallback
    var workspaces: [WorkspaceDraft] = []
    var view: SettingsView?

    private var flashTask: Task<Void, Never>?

    func load(client: APIClient, force: Bool) async {
        if hydrated && !force { return }
        if loading { return }
        loading = true
        loadError = ""
        defer { loading = false }
        do {
            let fetched = try await client.fetchSettings()
            hydrate(from: fetched)
        } catch {
            loadError = friendlyError(error.localizedDescription)
        }
    }

    func save(client: APIClient) async -> Bool {
        if saving { return false }
        saving = true
        defer { saving = false }
        let body = SettingsSaveBody(
            providers: providers,
            roles: roles,
            connectors: ConnectorSaveBody(servers: connectors),
            capabilities: capabilities,
            limits: limits,
            workspaces: workspaceMap()
        )
        do {
            let saved = try await client.saveSettings(body)
            reconcile(with: saved)
            dirty = false
            flashSaved()
            return true
        } catch {
            return false
        }
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
        capabilities = view.capabilities
        connectors = view.connectors.servers.map(ConnectorDraft.init(view:))
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

    private func reconcile(with saved: SettingsView) {
        view = saved
        capabilities = saved.capabilities
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
        let returnedConnectors = Dictionary(uniqueKeysWithValues: saved.connectors.servers.map { ($0.name, $0) })
        for index in connectors.indices {
            let name = connectors[index].name.trimmingCharacters(in: .whitespacesAndNewlines)
            guard let returned = returnedConnectors[name] else { continue }
            connectors[index].env = syncRows(connectors[index].env, with: returned.env)
            connectors[index].headers = syncRows(connectors[index].headers, with: returned.headers)
        }
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
