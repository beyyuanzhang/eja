"""A deterministic authored MCP plugin used to test sandboxed versions."""
import json
import os
import socket
import sys

REVISION = "one"


def allowed(action):
    try:
        action()
        return True
    except OSError:
        return False


def respond(request):
    method = request["method"]
    if method == "initialize":
        return {"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                "serverInfo": {"name": "test-plugin", "version": REVISION}}
    if method == "tools/list":
        return {"tools": [{"name": "format", "description": "Format the input",
                           "inputSchema": {"type": "object"}}]}
    if method == "tools/call":
        arguments = request["params"].get("arguments", {})
        path = arguments.get("private")
        port = arguments.get("port")
        output = {
            "value": REVISION + ":" + arguments.get("text", ""),
            "environment_leaked": any(key in os.environ for key in ("HOME", "EJA_PLUGIN_TEST_SECRET")),
        }
        if path:
            output["private_readable"] = allowed(lambda: open(path).close())
            output["private_writable"] = allowed(lambda: open(path, "a").close())
        if port:
            output["network_allowed"] = allowed(lambda: socket.create_connection(("127.0.0.1", port), timeout=1).close())
        return {"content": [{"type": "text", "text": output["value"]}],
                "structuredContent": output, "isError": arguments.get("fail", False)}
    return {}


for line in sys.stdin:
    request = json.loads(line)
    if "id" in request:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": respond(request)}), flush=True)
