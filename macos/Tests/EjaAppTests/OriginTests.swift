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
    func providerConfigAndLiveMcpConnectorRoundtrip() async throws {
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

        let cfg = HostConfigDTO(
            providers: [
                ProviderConfigDTO(id: "deepseek", name: "DeepSeek", kind: "openai", baseUrl: "https://api.deepseek.com/v1", apiKey: "sk-demo", models: ["deepseek-chat"]),
                ProviderConfigDTO(id: "anthropic", name: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com", apiKey: "sk-ant-demo", models: ["claude-sonnet-4-5"]),
            ],
            judge: ModelRefDTO(providerId: "anthropic", model: "claude-sonnet-4-5"),
            connectors: [
                ConnectorConfigDTO(
                    id: "echo",
                    name: "Echo MCP",
                    enabled: true,
                    transport: .stdio(command: "/usr/bin/python3", args: [mcpScript])
                ),
            ]
        )
        client.saveConfig(cfg)

        #expect(await waitUntil {
            client.isConfigured
                && client.config == cfg
                && client.connectorStatuses.first?.connected == true
                && client.connectorStatuses.first?.tools.first?.capability == "mcp.echo.echo"
        })

        client.shutdown()

        let restarted = HostClient(hostExecutableURL: hostBin, dataDir: dir)
        defer { restarted.shutdown() }

        #expect(await waitUntil {
            restarted.isReady
                && restarted.config == cfg
                && restarted.connectorStatuses.first?.connected == true
        })
    }
}
