"""Deterministic MCP peer for integration tests; no third-party dependencies."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

root = Path(sys.argv[1])
mode = sys.argv[2]
lock = threading.Lock()
pending = {}


def record(value):
    with lock:
        with (root / "events").open("a") as f:
            f.write(json.dumps(value) + "\n")


(root / "pid").write_text(str(os.getpid()))
if mode == "hang_init":
    time.sleep(60)
    sys.exit()
if mode == "descendant":
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    (root / "child_pid").write_text(str(child.pid))


def dispatch(message):
    record(message)
    method = message.get("method")
    ident = message.get("id")
    params = message.get("params", {})
    if method == "notifications/cancelled":
        event = pending.get(params["requestId"])
        if event:
            event.set()
    if ident is None:
        return None
    reply = {"jsonrpc": "2.0", "id": ident}
    if method == "server/discover":
        reply["error"] = {"code": -32601, "message": "legacy peer"}
    elif method == "initialize":
        reply["result"] = {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1"},
        }
    elif method == "tools/list":
        cursor = params.get("cursor")
        names = ["echo", "fail"] if cursor is None else ["wait", "big", "a.b", "a-b"]
        if mode == "collision":
            names = ["a.b", "a/b"]
        reply["result"] = {"tools": [
            {"name": name, "description": "test " + name,
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}
            for name in names
        ]}
        if cursor is None or mode == "repeat_cursor":
            reply["result"]["nextCursor"] = "page2"
    elif method == "tools/call":
        name = params["name"]
        text = params.get("arguments", {}).get("text", "")
        if name == "wait":
            event = threading.Event()
            if text != "ignore_cancel":
                pending[ident] = event
            event.wait(10)
            pending.pop(ident, None)
        if name == "big":
            text = "x" * 70000
        reply["result"] = {
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"node": text, "marker": os.getenv("MCP_MARKER", "")},
            "isError": name == "fail",
        }
    elif method == "ping":
        reply["result"] = {}
    else:
        reply["error"] = {"code": -32601, "message": "unknown method"}
    return reply


if mode == "http":
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send(self, status, value=None, sse=False, session=False):
            data = b""
            if value is not None:
                data = json.dumps(value).encode()
                if sse:
                    data = b"event: message\ndata: " + data + b"\n\n"
            self.send_response(status)
            self.send_header("Content-Type", "text/event-stream" if sse else "application/json")
            self.send_header("Content-Length", str(len(data)))
            if session:
                self.send_header("Mcp-Session-Id", "fixture-session")
            self.end_headers()
            try:
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def do_POST(self):
            message = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            record({"headers": dict(self.headers), "method": "http_headers"})
            result = dispatch(message)
            text = message.get("params", {}).get("arguments", {}).get("text")
            if text == "http500":
                self.send(500)
            elif text == "expired":
                self.send(404)
            else:
                self.send(200 if result else 202, result, sse=text == "sse",
                          session=message.get("method") == "initialize")

        def do_GET(self):
            self.send(405)

        def do_DELETE(self):
            record({"method": "http_delete"})
            self.send(200)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    (root / "url").write_text("http://127.0.0.1:" + str(server.server_port) + "/mcp")
    server.serve_forever()
else:
    def serve(message):
        reply = dispatch(message)
        if reply:
            with lock:
                print(json.dumps(reply), flush=True)

    for line in sys.stdin:
        message = json.loads(line)
        if message.get("method") == "tools/call":
            threading.Thread(target=serve, args=(message,), daemon=True).start()
        else:
            serve(message)
