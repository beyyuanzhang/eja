import Foundation
import Testing
@testable import ejaClient

struct ejaClientTests {
    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("eja-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private func resolveHostBinary() -> URL {
        if let env = ProcessInfo.processInfo.environment["eja_HOST_BIN"] {
            let url = URL(fileURLWithPath: env)
            if FileManager.default.isExecutableFile(atPath: url.path) { return url }
        }
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        for rel in ["target/release/eja-host", "target/debug/eja-host"] {
            let candidate = repoRoot.appendingPathComponent(rel)
            if FileManager.default.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return repoRoot.appendingPathComponent("target/release/eja-host")
    }

    @MainActor
    private func waitUntil(timeoutSeconds: Double = 5.0, condition: () -> Bool) async -> Bool {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if condition() { return true }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        return condition()
    }

    @Test
    @MainActor
    func providerConfigAndUnifiedCapabilitiesRoundtrip() async throws {
        let hostBin = resolveHostBinary()
        #expect(FileManager.default.isExecutableFile(atPath: hostBin.path))

        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }

        let mcpScript = dir.path + "/echo_mcp.py"
        let scriptBody = """
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    meta = msg["params"]["_meta"]
    assert meta["io.modelcontextprotocol/protocolVersion"] == "2026-07-28"
    assert meta["io.modelcontextprotocol/clientCapabilities"] == {}
    if method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"resultType":"complete","tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"resultType":"complete","content":[{"type":"text","text":"ok"}]}}), flush=True)
"""
        try scriptBody.write(toFile: mcpScript, atomically: true, encoding: .utf8)

        let client = HostClient(hostExecutableURL: hostBin, dataDir: dir)
        defer { client.shutdown() }

        #expect(await waitUntil { client.isReady })
        #expect(client.status == "idle")
        #expect(client.capabilityStatuses.first?.id == "shell")
        #expect(client.capabilityStatuses.first?.connected == true)

        let cfg = HostConfigDTO(
            providers: [
                ProviderConfigDTO(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", apiKey: "sk-demo", models: ["deepseek-chat"]),
                ProviderConfigDTO(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", apiKey: "sk-ant-demo", models: ["claude-sonnet-4-5"]),
            ],
            judge: ModelRefDTO(providerId: "anthropic", model: "claude-sonnet-4-5"),
            capabilities: [
                CapabilityConfigDTO(
                    id: "shell",
                    name: "Shell",
                    enabled: true,
                    transport: .builtin()
                ),
                CapabilityConfigDTO(
                    id: "echo",
                    name: "Echo MCP",
                    enabled: true,
                    transport: .stdio(command: "/usr/bin/python3", args: [mcpScript])
                ),
            ]
        )
        client.saveConfig(providers: cfg.providers, judge: cfg.judge, capabilities: cfg.capabilities)

        var expectedRedacted = cfg
        for idx in expectedRedacted.providers.indices {
            expectedRedacted.providers[idx].hasApiKey = !expectedRedacted.providers[idx].apiKey.isEmpty
            expectedRedacted.providers[idx].apiKey = ""
        }

        #expect(await waitUntil {
            client.isConfigured
                && client.config == expectedRedacted
                && client.capabilityStatuses.count == 2
                && client.capabilityStatuses[1].connected == true
                && client.capabilityStatuses[1].tools.first?.capability == "cap.echo.echo"
        })

        client.shutdown()

        let restarted = HostClient(hostExecutableURL: hostBin, dataDir: dir)
        defer { restarted.shutdown() }

        #expect(await waitUntil {
            restarted.isReady
                && restarted.config == expectedRedacted
                && restarted.capabilityStatuses.count == 2
                && restarted.capabilityStatuses[1].connected == true
        })

        var renamedProviders = restarted.config.providers
        renamedProviders[0].name = "DeepSeek Updated"
        restarted.saveConfig(providers: renamedProviders, judge: restarted.config.judge)

        #expect(await waitUntil {
            restarted.config.providers.first?.name == "DeepSeek Updated"
                && restarted.config.capabilities == expectedRedacted.capabilities
                && restarted.capabilityStatuses[1].connected == true
        })

        var capabilities = restarted.config.capabilities
        capabilities[1].enabled = false
        var confirmed: HostConfigDTO?
        restarted.saveConfig(capabilities: capabilities) { confirmed = $0 }
        #expect(await waitUntil { confirmed != nil })
        #expect(confirmed?.capabilities[1].enabled == false)
        #expect(restarted.capabilityStatuses[1].enabled == false)
        #expect(restarted.capabilityStatuses[1].tools.isEmpty)
    }

    @Test
    @MainActor
    func singleInstanceConflictEmitsAlreadyRunningWithoutError() async throws {
        let hostBin = resolveHostBinary()
        #expect(FileManager.default.isExecutableFile(atPath: hostBin.path))

        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }

        let first = HostClient(hostExecutableURL: hostBin, dataDir: dir)
        defer { first.shutdown() }
        #expect(await waitUntil { first.isReady })

        let second = HostClient(hostExecutableURL: hostBin, dataDir: dir)
        defer { second.shutdown() }
        #expect(await waitUntil { second.alreadyRunning })
        try? await Task.sleep(nanoseconds: 150_000_000)
        #expect(second.errorBanner == nil)
    }

    @Test
    @MainActor
    func rejectsOlderHostProtocol() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let client = HostClient(hostExecutableURL: URL(fileURLWithPath: "/bin/cat"), dataDir: dir)
        defer { client.shutdown() }

        client.handleEventLine(
            Data(#"{"event":"state","protocol_version":1,"status":"idle","status_detail":"在线","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[]}"#.utf8)
        )

        #expect(!client.isReady)
        #expect(client.errorBanner?.contains("协议不匹配") == true)
    }

    @Test
    @MainActor
    func streamingDeltaAndActivityEventsConvergeOnState() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let client = HostClient(hostExecutableURL: URL(fileURLWithPath: "/bin/cat"), dataDir: dir)
        defer { client.shutdown() }

        client.handleEventLine(
            Data(#"{"event":"state","protocol_version":2,"status":"thinking","status_detail":"思考中…","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[]}"#.utf8)
        )
        #expect(client.status == "thinking")
        #expect(client.statusDetail == "思考中…")
        #expect(client.streamingText == "")

        client.handleEventLine(Data(#"{"event":"delta","text":"你好"}"#.utf8))
        client.handleEventLine(Data(#"{"event":"delta","text":"，世界"}"#.utf8))
        #expect(client.streamingText == "你好，世界")

        client.handleEventLine(Data(#"{"event":"activity","status_detail":"执行 Shell"}"#.utf8))
        #expect(client.statusDetail == "执行 Shell")
        #expect(client.streamingText == "")

        client.handleEventLine(Data(#"{"event":"delta","text":"最终回复"}"#.utf8))
        #expect(client.streamingText == "最终回复")

        client.handleEventLine(
            Data(#"{"event":"state","protocol_version":2,"status":"idle","status_detail":"在线","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[{"id":1,"kind":"eja","text":"最终回复"}]}"#.utf8)
        )
        #expect(client.status == "idle")
        #expect(client.statusDetail == "在线")
        #expect(client.streamingText == "")
        #expect(client.stream.last?.text == "最终回复")
    }

    @Test
    @MainActor
    func historyPagesPrependAndStateKeepsLoadedItems() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let client = HostClient(hostExecutableURL: URL(fileURLWithPath: "/bin/cat"), dataDir: dir)
        defer { client.shutdown() }

        client.handleEventLine(
            Data(#"{"event":"state","protocol_version":2,"status":"idle","status_detail":"在线","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[{"id":5,"kind":"event","text":"shell: ls","detail":"ok","outcome_status":"observed"}],"concerns":[{"id":2,"text":"挂念"}],"wakes":[{"id":"w-1","fire_at_unix":9,"fire_at_display":"09:00","reason":"喝水"}]}"#.utf8)
        )
        #expect(client.stream.first?.detail == "ok")
        #expect(client.concerns.first?.text == "挂念")
        #expect(client.wakes.first?.reason == "喝水")
        #expect(client.canLoadEarlier)

        client.handleEventLine(Data(#"{"event":"history","items":[{"id":3,"kind":"event","text":"定时唤醒：喝水"}],"more":false}"#.utf8))
        #expect(client.stream.map(\.id) == [3, 5])
        #expect(!client.canLoadEarlier)

        client.handleEventLine(
            Data(#"{"event":"state","protocol_version":2,"status":"idle","status_detail":"在线","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[{"id":5,"kind":"event","text":"shell: ls"},{"id":6,"kind":"person","text":"嗨"}],"concerns":[],"wakes":[]}"#.utf8)
        )
        #expect(client.stream.map(\.id) == [3, 5, 6])
        #expect(client.wakes.isEmpty)
    }
}
