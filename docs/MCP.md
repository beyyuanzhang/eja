# MCP tools

EJA owns the clients. AJA receives tool definitions and sends tool calls over its
existing IPC. One host tool registry holds connections for all nodes; freezing a
node retains no node-owned MCP client, server or thread.

## Configuration

The CLI loads `<dir>/mcp.json` when present, or the exact `--mcp-config FILE` path.
It does not search personal or other project configuration directories.
No config means file tools only. A configured server that fails to initialize or
list its granted tools fails startup, rather than silently removing capabilities.

```json
{
  "mcpServers": {
    "local": {
      "command": "/absolute/path/to/server",
      "args": ["--stdio"],
      "env": {"SERVICE_TOKEN": "${MY_SERVICE_TOKEN}"},
      "tools": ["search", "fetch"],
      "timeout_ms": 30000
    },
    "remote": {
      "url": "https://your-service.example/mcp",
      "headers": {"Authorization": "Bearer ${MY_SERVICE_TOKEN}"},
      "tools": ["search"]
    }
  }
}
```

Each server specifies exactly one `command` or `url`. Unknown fields are errors.
Names are 1–24 ASCII letters, digits, underscores or hyphens. Optional `enabled: false`
skips a server. `tools` lists exact remote names: omitted grants the startup
catalog, `[]` grants none; a missing configured tool is an error. The catalog is
fixed for the host lifetime, so reconnect/restart is required for catalog changes.

Stdio programs run directly, without a shell. The default working directory is
the project; optional `cwd` is absolute or relative to the config file. Only
`PATH`, `HOME`, `TMPDIR`, `TMP`, `LANG`, `SYSTEMROOT` and explicit `env` are passed.
Only `env` values and HTTP header values expand `${VARIABLE}`; missing variables
are errors. Arguments and commands are literal. Server stderr is inherited by the host;
stdout must contain MCP JSON lines only.

HTTP uses Streamable HTTP, accepts JSON and SSE responses, and supports configured
headers. Protocol/session headers belong to the SDK. URL credentials and
redirects are rejected. This implementation does not perform interactive OAuth.

```bash
eja --dir .eja tools
eja --dir .eja --mcp-config /path/to/mcp.json turn "use the connector"
```

Tool names become `mcp__<server>__<tool>`; unsupported characters become `_`.
Names longer than 64 bytes get a stable hash suffix. Collisions are errors.
The `tools` command prints the actual model-visible names and schemas, without
printing host config or authentication headers.

## Lifetime and bounds

The official [Rust SDK (`rmcp`)](https://github.com/modelcontextprotocol/rust-sdk)
handles protocol negotiation and request correlation.
EJA supplies no sampling, elicitation or task-continuation capabilities. This
adapter exposes MCP tools; it does not add a second agent loop, plugin system,
resource browser or prompt registry.

- Maximum 32 configured servers, 256 tools/pages, 256 KiB config/catalog/stdio frame,
  frame-sized (256 KiB) returned tool result; the transport allows one frame of
  payload plus envelope headroom. A larger result is refused with a clear error.
- Per-server timeout defaults to 30 seconds (maximum 300 seconds), including
  initialization and discovery. Calls use the smaller of that timeout and the
  node's remaining deadline. Cancellation is checked while waiting, and sends
  an MCP cancellation notification for a pending call.
- Results retain MCP `content`, `structuredContent` and `isError`; remote resource
  references stay data and are not automatically fetched or made local artifacts.
- `read_only` lists server tool names the host declares safe for EJA to observe.
  EJA can also observe any tool the server itself annotated `readOnlyHint`. The
  host is the authority here: declare only tools that cannot change state, because
  EJA will call them on its own.
- An expired session is reinitialized and that request retried once, because a
  rejected session means the request did not execute. Other HTTP failures are
  never replayed.
- Shutdown closes HTTP sessions and kills/waits for owned stdio processes. On
  Unix, ordinary descendants in their server's process group are killed too.
  Initialization failures clean up servers already started.

## Trust boundary

There is no OS sandbox. Use trusted MCP servers: local servers have the host
user's OS permissions, and remote tools have their configured service authority.
File-tool path restrictions and AJA grants are application boundaries. They do
not constrain a separate MCP server's internal code. MCP cancellation is a
request to the server; it cannot guarantee rollback or termination of remote
work, and a deliberately detached process requires its own lifecycle management.

The current CLI host lives for one command. A future long-running app can retain
the same `HostTools` for its application lifetime without changing AJA.
