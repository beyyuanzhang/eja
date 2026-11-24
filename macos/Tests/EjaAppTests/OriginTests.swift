import Foundation
import Testing
@testable import EjaClient

struct OriginTests {
    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("eja-origin-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private func resolveHostBinary() -> URL {
        if let env = ProcessInfo.processInfo.environment["EJA_HOST_BIN"] {
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
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"echo","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"ok"}]}}), flush=True)
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
        client.saveConfig(cfg)

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
        restarted.saveConfigPatch(providers: renamedProviders, judge: restarted.config.judge)

        #expect(await waitUntil {
            restarted.config.providers.first?.name == "DeepSeek Updated"
                && restarted.config.capabilities == expectedRedacted.capabilities
                && restarted.capabilityStatuses[1].connected == true
        })
    }

    @Test
    @MainActor
    func streamingDeltaAndActivityEventsConvergeOnState() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let client = HostClient(hostExecutableURL: URL(fileURLWithPath: "/bin/cat"), dataDir: dir)
        defer { client.shutdown() }

        client.handleEventLine(
            Data(#"{"event":"state","status":"thinking","status_detail":"思考中…","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[]}"#.utf8)
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
            Data(#"{"event":"state","status":"idle","status_detail":"在线","data_dir":"/tmp/eja","configured":true,"config":{"providers":[],"judge":{"provider_id":"","model":""},"capabilities":[]},"capability_statuses":[],"stream":[{"id":1,"kind":"eja","text":"最终回复"}]}"#.utf8)
        )
        #expect(client.status == "idle")
        #expect(client.statusDetail == "在线")
        #expect(client.streamingText == "")
        #expect(client.stream.last?.text == "最终回复")
    }
}
