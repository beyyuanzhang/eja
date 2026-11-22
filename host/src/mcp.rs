use crate::config::{ConnectorConfig, ConnectorTransport};
use crate::sandbox::{kill_process_tree, Sandbox};
use eja_core::Outcome;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    mpsc::{self, Receiver},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

const MCP_TIMEOUT: Duration = Duration::from_secs(30);
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTCOME_BYTES: usize = 32 * 1024;
const MAX_STDERR_BYTES: usize = 4 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum RequestError {
    Failed(String),
    Unknown(String),
}

impl RequestError {
    fn into_message(self) -> String {
        match self {
            Self::Failed(message) | Self::Unknown(message) => message,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolSummary {
    pub name: String,
    pub description: String,
    pub capability: String,
}

#[derive(Debug, Clone)]
pub struct McpTool {
    pub connector_id: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capability: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorStatus {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools: Vec<McpToolSummary>,
}

struct StdioConnection {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl StdioConnection {
    fn connect(
        sandbox: &Sandbox,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut cmd = sandbox.command(command, args);
        cmd.envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|err| format!("启动 MCP 进程失败 ({command}): {err}"))?;
        let stdin = child.stdin.take().ok_or("无法打开 MCP stdin")?;
        let stdout = child.stdout.take().ok_or("无法打开 MCP stdout")?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err_pipe) = child.stderr.take() {
            let buf = Arc::clone(&stderr);
            thread::spawn(move || {
                let reader = BufReader::new(err_pipe);
                for line in reader.lines().map_while(Result::ok) {
                    let mut guard = buf.lock().unwrap();
                    if !guard.is_empty() {
                        guard.push('\n');
                    }
                    guard.push_str(&line);
                    if guard.len() > MAX_STDERR_BYTES {
                        let drop_to = guard.len() - MAX_STDERR_BYTES;
                        let start = guard.ceil_char_boundary(drop_to);
                        guard.drain(..start);
                    }
                }
            });
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if tx.send(text).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut conn = Self {
            child,
            stdin,
            rx,
            stderr,
            next_id: 1,
        };
        conn.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "eja", "version": env!("CARGO_PKG_VERSION") }
            }),
            MCP_TIMEOUT,
        )
        .map_err(RequestError::into_message)?;
        conn.notify("notifications/initialized", json!({}))?;
        Ok(conn)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let line = serde_json::to_string(&msg).map_err(|err| err.to_string())?;
        writeln!(self.stdin, "{line}").map_err(|err| format!("写入 MCP 失败: {err}"))?;
        self.stdin.flush().map_err(|err| err.to_string())
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let line =
            serde_json::to_string(&msg).map_err(|err| RequestError::Failed(err.to_string()))?;
        writeln!(self.stdin, "{line}")
            .map_err(|err| RequestError::Failed(format!("写入 MCP 失败: {err}")))?;
        self.stdin
            .flush()
            .map_err(|err| RequestError::Unknown(format!("刷新 MCP 请求失败: {err}")))?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(RequestError::Unknown(
                    self.with_stderr(format!("MCP 请求超时 ({method})")),
                ));
            }
            let raw = self.rx.recv_timeout(remaining).map_err(|_| {
                RequestError::Unknown(
                    self.with_stderr(format!("MCP 响应超时或进程退出 ({method})")),
                )
            })?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if val.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = val.get("error") {
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("MCP 返回错误");
                return Err(RequestError::Failed(message.to_string()));
            }
            return Ok(val.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn with_stderr(&self, message: String) -> String {
        let err = self.stderr.lock().unwrap().trim().to_string();
        if err.is_empty() {
            message
        } else {
            format!("{message}\n[stderr]\n{err}")
        }
    }
}

impl Drop for StdioConnection {
    fn drop(&mut self) {
        kill_process_tree(&mut self.child);
    }
}

struct HttpConnection {
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpConnection {
    fn connect(url: &str, headers: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut conn = Self {
            url: url.trim().to_string(),
            headers: headers.clone(),
            session_id: None,
            next_id: 1,
        };
        conn.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "eja", "version": env!("CARGO_PKG_VERSION") }
            }),
            MCP_TIMEOUT,
        )
        .map_err(RequestError::into_message)?;
        let _ = conn.post_raw(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
            Duration::from_secs(5),
        );
        Ok(conn)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let (resp_headers, body) = self.post_raw(&payload, timeout)?;
        for line in resp_headers.lines() {
            if let Some((key, val)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("mcp-session-id") {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() {
                        self.session_id = Some(trimmed.to_string());
                    }
                }
            }
        }
        let parsed = parse_http_jsonrpc(&body).map_err(RequestError::Unknown)?;
        if let Some(err) = parsed.get("error") {
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("MCP HTTP 返回错误");
            return Err(RequestError::Failed(message.to_string()));
        }
        Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
    }

    fn post_raw(
        &self,
        payload: &Value,
        timeout: Duration,
    ) -> Result<(String, String), RequestError> {
        let body_bytes =
            serde_json::to_vec(payload).map_err(|err| RequestError::Failed(err.to_string()))?;
        let mut cmd = Command::new("/usr/bin/curl");
        cmd.args([
            "-sS",
            "-i",
            "-X",
            "POST",
            "--max-time",
            &timeout.as_secs().max(1).to_string(),
            "-H",
            "Content-Type: application/json",
            "-H",
            "Accept: application/json, text/event-stream",
        ]);
        if let Some(sid) = &self.session_id {
            cmd.arg("-H").arg(format!("Mcp-Session-Id: {sid}"));
        }
        for (key, value) in &self.headers {
            cmd.arg("-H").arg(format!("{key}: {value}"));
        }
        cmd.args(["--data-binary", "@-", &self.url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|err| RequestError::Failed(format!("调用 curl 失败: {err}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            thread::spawn(move || {
                let _ = stdin.write_all(&body_bytes);
            });
        }
        let output = child
            .wait_with_output()
            .map_err(|err| RequestError::Unknown(format!("等待 MCP HTTP 响应失败: {err}")))?;
        if !output.status.success() {
            let err_text = String::from_utf8_lossy(&output.stderr);
            let message = format!("MCP HTTP 请求失败: {}", err_text.trim());
            return Err(match output.status.code() {
                Some(3 | 6 | 7) => RequestError::Failed(message),
                _ => RequestError::Unknown(message),
            });
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let (headers, body) = split_http_response(&raw)?;
        Ok((headers.to_string(), body.to_string()))
    }
}

fn split_http_response(raw: &str) -> Result<(&str, &str), RequestError> {
    let mut rest = raw;
    let mut last_headers = "";
    let mut status_code: Option<u16> = None;

    while rest.starts_with("HTTP/") {
        let Some((head, tail)) = split_once_header_end(rest) else {
            return Err(RequestError::Unknown(format!(
                "无效 MCP HTTP 响应: {}",
                truncate_utf8(rest, 240)
            )));
        };
        let code = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        last_headers = head;
        status_code = Some(code);
        rest = tail;
        if !(100..200).contains(&code) {
            break;
        }
    }

    if let Some(code) = status_code {
        if !(200..300).contains(&code) {
            let message = format!("MCP HTTP {code}: {}", truncate_utf8(rest.trim(), 240));
            return Err(match code {
                408 | 502 | 503 | 504 => RequestError::Unknown(message),
                _ => RequestError::Failed(message),
            });
        }
    } else {
        return Err(RequestError::Unknown(format!(
            "缺少 MCP HTTP 状态行: {}",
            truncate_utf8(raw.trim(), 240)
        )));
    }
    Ok((last_headers, rest))
}

fn split_once_header_end(input: &str) -> Option<(&str, &str)> {
    match (input.find("\r\n\r\n"), input.find("\n\n")) {
        (Some(a), Some(b)) if a <= b => Some((&input[..a], &input[a + 4..])),
        (Some(_), Some(b)) => Some((&input[..b], &input[b + 2..])),
        (Some(a), None) => Some((&input[..a], &input[a + 4..])),
        (None, Some(b)) => Some((&input[..b], &input[b + 2..])),
        (None, None) => None,
    }
}

fn parse_http_jsonrpc(body: &str) -> Result<Value, String> {
    let trimmed = body.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        return Ok(val);
    }
    let mut last_json = None;
    for line in trimmed.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let chunk = data.trim();
            if let Ok(val) = serde_json::from_str::<Value>(chunk) {
                last_json = Some(val);
            }
        }
    }
    last_json.ok_or_else(|| format!("无法解析 MCP HTTP 响应: {}", truncate_utf8(trimmed, 240)))
}

enum ActiveConnection {
    Stdio(StdioConnection),
    Http(HttpConnection),
}

impl ActiveConnection {
    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        match self {
            Self::Stdio(conn) => conn.request(method, params, timeout),
            Self::Http(conn) => conn.request(method, params, timeout),
        }
    }
}

pub struct ConnectorPool {
    sandbox: Sandbox,
    connections: HashMap<String, ActiveConnection>,
    statuses: Vec<ConnectorStatus>,
    tools: Vec<McpTool>,
}

impl ConnectorPool {
    pub fn new(sandbox: Sandbox) -> Self {
        Self {
            sandbox,
            connections: HashMap::new(),
            statuses: Vec::new(),
            tools: Vec::new(),
        }
    }

    pub fn reload(&mut self, configs: &[ConnectorConfig]) {
        self.connections.clear();
        self.statuses.clear();
        self.tools.clear();

        for cfg in configs {
            if !cfg.enabled {
                self.statuses.push(ConnectorStatus {
                    id: cfg.id.clone(),
                    name: cfg.name.clone(),
                    enabled: false,
                    connected: false,
                    error: None,
                    tools: Vec::new(),
                });
                continue;
            }

            let connected = match &cfg.transport {
                ConnectorTransport::Stdio { command, args, env } => {
                    StdioConnection::connect(&self.sandbox, command, args, env)
                        .map(ActiveConnection::Stdio)
                }
                ConnectorTransport::Http { url, headers } => {
                    HttpConnection::connect(url, headers).map(ActiveConnection::Http)
                }
            };

            match connected {
                Ok(mut conn) => match conn
                    .request("tools/list", json!({}), MCP_TIMEOUT)
                    .map_err(RequestError::into_message)
                {
                    Ok(result) => {
                        let raw_tools = result
                            .get("tools")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let mut summaries = Vec::new();
                        let safe_id = sanitize_id(&cfg.id);
                        for item in raw_tools {
                            let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                                continue;
                            };
                            let description = item
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input_schema = item
                                .get("inputSchema")
                                .cloned()
                                .filter(|v| v.is_object())
                                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                            let capability = format!("mcp.{safe_id}.{name}");
                            summaries.push(McpToolSummary {
                                name: name.to_string(),
                                description: description.clone(),
                                capability: capability.clone(),
                            });
                            self.tools.push(McpTool {
                                connector_id: cfg.id.clone(),
                                name: name.to_string(),
                                description,
                                input_schema,
                                capability,
                            });
                        }
                        self.connections.insert(cfg.id.clone(), conn);
                        self.statuses.push(ConnectorStatus {
                            id: cfg.id.clone(),
                            name: cfg.name.clone(),
                            enabled: true,
                            connected: true,
                            error: None,
                            tools: summaries,
                        });
                    }
                    Err(err) => {
                        self.statuses.push(ConnectorStatus {
                            id: cfg.id.clone(),
                            name: cfg.name.clone(),
                            enabled: true,
                            connected: false,
                            error: Some(err),
                            tools: Vec::new(),
                        });
                    }
                },
                Err(err) => {
                    self.statuses.push(ConnectorStatus {
                        id: cfg.id.clone(),
                        name: cfg.name.clone(),
                        enabled: true,
                        connected: false,
                        error: Some(err),
                        tools: Vec::new(),
                    });
                }
            }
        }
    }

    pub fn statuses(&self) -> Vec<ConnectorStatus> {
        self.statuses.clone()
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.tools.clone()
    }

    pub fn call(&mut self, capability: &str, input: &Value) -> Outcome {
        let Some(tool) = self
            .tools
            .iter()
            .find(|item| item.capability == capability)
            .cloned()
        else {
            return Outcome::Failed(format!("未找到 MCP 工具: {capability}"));
        };
        let Some(conn) = self.connections.get_mut(&tool.connector_id) else {
            return Outcome::Failed(format!("MCP 连接器未连接: {}", tool.connector_id));
        };
        let args = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        match conn.request(
            "tools/call",
            json!({ "name": tool.name, "arguments": args }),
            TOOL_TIMEOUT,
        ) {
            Ok(result) => {
                let is_error = result
                    .get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let text = extract_mcp_result_text(&result);
                let bounded = truncate_utf8(&text, MAX_OUTCOME_BYTES);
                if is_error {
                    Outcome::Failed(bounded)
                } else {
                    Outcome::Observed(bounded)
                }
            }
            Err(RequestError::Failed(err)) => {
                Outcome::Failed(truncate_utf8(&err, MAX_OUTCOME_BYTES))
            }
            Err(RequestError::Unknown(err)) => {
                Outcome::Unknown(truncate_utf8(&err, MAX_OUTCOME_BYTES))
            }
        }
    }
}

fn extract_mcp_result_text(result: &Value) -> String {
    if let Some(items) = result.get("content").and_then(|v| v.as_array()) {
        let parts: Vec<String> = items
            .iter()
            .filter_map(|block| {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    Some(text.to_string())
                } else {
                    serde_json::to_string(block).ok()
                }
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    serde_json::to_string(result).unwrap_or_else(|_| "ok".to_string())
}

fn sanitize_id(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "mcp".to_string()
    } else {
        cleaned
    }
}

pub fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &input[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_http_response_preserves_sse_blank_lines_and_handles_continue() {
        let raw = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nMcp-Session-Id: s1\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\n\r\n";
        let (headers, body) = split_http_response(raw).unwrap();
        assert!(headers.contains("Mcp-Session-Id: s1"));
        let parsed = parse_http_jsonrpc(body).unwrap();
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn split_http_response_distinguishes_unknown_gateway_and_explicit_errors() {
        let gateway = "HTTP/1.1 502 Bad Gateway\r\n\r\nupstream failed";
        assert!(matches!(
            split_http_response(gateway),
            Err(RequestError::Unknown(ref msg)) if msg.contains("502") && msg.contains("upstream failed")
        ));

        let client = "HTTP/1.1 400 Bad Request\r\n\r\ninvalid arguments";
        assert!(matches!(
            split_http_response(client),
            Err(RequestError::Failed(ref msg)) if msg.contains("400") && msg.contains("invalid arguments")
        ));
    }

    #[test]
    fn stdio_disconnect_maps_to_unknown_and_jsonrpc_error_maps_to_failed() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path().join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        let script = dir.path().join("flaky_mcp.py");
        std::fs::write(
            &script,
            r#"import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"op","description":"Op","inputSchema":{"type":"object"}}]}}), flush=True)
    elif method == "tools/call":
        mode = msg["params"]["arguments"].get("mode")
        if mode == "err":
            print(json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32000,"message":"rejected"}}), flush=True)
        else:
            sys.exit(0)
"#,
        )
        .unwrap();

        let mut pool = ConnectorPool::new(Sandbox::new(&core_dir).unwrap());
        pool.reload(&[ConnectorConfig {
            id: "flaky".into(),
            name: "Flaky".into(),
            enabled: true,
            transport: ConnectorTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            },
        }]);

        assert_eq!(
            pool.call("mcp.flaky.op", &json!({ "mode": "err" })),
            Outcome::Failed("rejected".into())
        );
        assert!(matches!(
            pool.call("mcp.flaky.op", &json!({ "mode": "drop" })),
            Outcome::Unknown(ref msg) if msg.contains("MCP 响应超时或进程退出")
        ));
    }
}
