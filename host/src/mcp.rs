use crate::config::{ConnectorConfig, ConnectorTransport};
use crate::http::{post_bytes, truncate_utf8, HttpError, HttpResponse};
use crate::sandbox::{kill_process_tree, Sandbox};
use eja_core::Outcome;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
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

impl McpTool {
    fn summary(&self) -> McpToolSummary {
        McpToolSummary {
            name: self.name.clone(),
            description: self.description.clone(),
            capability: self.capability.clone(),
        }
    }
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

impl ConnectorStatus {
    fn from_cfg(
        cfg: &ConnectorConfig,
        connected: bool,
        error: Option<String>,
        tools: &[McpTool],
    ) -> Self {
        Self {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            enabled: cfg.enabled,
            connected,
            error,
            tools: tools.iter().map(McpTool::summary).collect(),
        }
    }
}

fn init_params() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": { "name": "eja", "version": env!("CARGO_PKG_VERSION") }
    })
}

struct StdioConnection {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
    broken: bool,
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
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut conn = Self {
            child,
            stdin,
            rx,
            stderr,
            next_id: 1,
            broken: false,
        };
        conn.request("initialize", init_params(), MCP_TIMEOUT)
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
        if let Err(err) = writeln!(self.stdin, "{line}") {
            self.broken = true;
            return Err(RequestError::Failed(format!("写入 MCP 失败: {err}")));
        }
        if let Err(err) = self.stdin.flush() {
            self.broken = true;
            return Err(RequestError::Unknown(format!("刷新 MCP 请求失败: {err}")));
        }

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                self.broken = true;
                return Err(RequestError::Unknown(
                    self.with_stderr(format!("MCP 请求超时 ({method})")),
                ));
            }
            let Ok(raw) = self.rx.recv_timeout(remaining) else {
                self.broken = true;
                return Err(RequestError::Unknown(
                    self.with_stderr(format!("MCP 响应超时或进程退出 ({method})")),
                ));
            };
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
            return extract_jsonrpc_result(&val, "MCP 返回错误");
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
        conn.request("initialize", init_params(), MCP_TIMEOUT)
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
        let parsed = parse_http_jsonrpc(&body, id).map_err(RequestError::Unknown)?;
        extract_jsonrpc_result(&parsed, "MCP HTTP 返回错误")
    }

    fn post_raw(
        &self,
        payload: &Value,
        timeout: Duration,
    ) -> Result<(String, String), RequestError> {
        let body_bytes =
            serde_json::to_vec(payload).map_err(|err| RequestError::Failed(err.to_string()))?;
        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "Accept".to_string(),
                "application/json, text/event-stream".to_string(),
            ),
        ];
        if let Some(sid) = &self.session_id {
            headers.push(("Mcp-Session-Id".to_string(), sid.clone()));
        }
        for (key, value) in &self.headers {
            headers.push((key.clone(), value.clone()));
        }
        let resp = post_bytes(&self.url, &headers, &body_bytes, timeout.as_secs().max(1)).map_err(
            |err| match err {
                HttpError::Launch(msg) => RequestError::Failed(msg),
                HttpError::Wait(msg) => RequestError::Unknown(format!("MCP {msg}")),
                HttpError::Transport { exit_code, message } => {
                    let msg = format!("MCP HTTP 请求失败: {message}");
                    match exit_code {
                        Some(3 | 6 | 7) => RequestError::Failed(msg),
                        _ => RequestError::Unknown(msg),
                    }
                }
                HttpError::InvalidResponse(msg) => RequestError::Unknown(msg),
            },
        )?;
        check_mcp_http_status(resp)
    }
}

fn check_mcp_http_status(resp: HttpResponse) -> Result<(String, String), RequestError> {
    if !(200..300).contains(&resp.status) {
        let message = format!(
            "MCP HTTP {}: {}",
            resp.status,
            truncate_utf8(resp.body.trim(), 240)
        );
        return Err(match resp.status {
            400..=407 | 409..=498 => RequestError::Failed(message),
            _ => RequestError::Unknown(message),
        });
    }
    Ok((resp.headers, resp.body))
}

#[cfg(test)]
fn split_http_response(raw: &str) -> Result<(String, String), RequestError> {
    let resp = crate::http::parse_http_response(raw).map_err(|err| match err {
        HttpError::InvalidResponse(msg) => RequestError::Unknown(msg),
        other => RequestError::Unknown(format!("{other:?}")),
    })?;
    check_mcp_http_status(resp)
}

fn extract_jsonrpc_result(val: &Value, default_error: &str) -> Result<Value, RequestError> {
    if let Some(err) = val.get("error").filter(|v| !v.is_null()) {
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(default_error);
        return Err(RequestError::Failed(message.to_string()));
    }
    if let Some(result) = val.get("result") {
        return Ok(result.clone());
    }
    Err(RequestError::Unknown(format!(
        "MCP 响应缺少 result 字段: {}",
        truncate_utf8(&val.to_string(), 240)
    )))
}

fn find_matching_jsonrpc(val: Value, id: u64) -> Option<Value> {
    let matches_id =
        |item: &Value| item.is_object() && item.get("id").and_then(Value::as_u64) == Some(id);
    if matches_id(&val) {
        return Some(val);
    }
    if let Value::Array(items) = val {
        return items.into_iter().find(matches_id);
    }
    None
}

fn parse_http_jsonrpc(body: &str, id: u64) -> Result<Value, String> {
    let trimmed = body.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        return find_matching_jsonrpc(val, id).ok_or_else(|| {
            format!(
                "MCP HTTP 响应缺少匹配请求 ID ({id}) 的结果: {}",
                truncate_utf8(trimmed, 240)
            )
        });
    }
    let mut matched: Option<Value> = None;
    let mut event_data = String::new();
    let flush_event = |buf: &mut String, out: &mut Option<Value>| {
        if buf.is_empty() {
            return;
        }
        if let Ok(val) = serde_json::from_str::<Value>(buf.trim()) {
            if let Some(found) = find_matching_jsonrpc(val, id) {
                *out = Some(found);
            }
        }
        buf.clear();
    };
    for line in trimmed.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            flush_event(&mut event_data, &mut matched);
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            let chunk = data.trim();
            if let Ok(val) = serde_json::from_str::<Value>(chunk) {
                if let Some(found) = find_matching_jsonrpc(val, id) {
                    matched = Some(found);
                    event_data.clear();
                    continue;
                }
            }
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(chunk);
        }
    }
    flush_event(&mut event_data, &mut matched);
    matched.ok_or_else(|| {
        format!(
            "MCP HTTP 响应缺少匹配请求 ID ({id}) 的结果: {}",
            truncate_utf8(trimmed, 240)
        )
    })
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

    fn is_alive(&mut self) -> bool {
        match self {
            Self::Stdio(conn) => !conn.broken && matches!(conn.child.try_wait(), Ok(None)),
            Self::Http(_) => true,
        }
    }
}

struct ConnectedEntry {
    transport: ConnectorTransport,
    connection: ActiveConnection,
    tools: Vec<McpTool>,
}

pub struct ConnectorPool {
    sandbox: Sandbox,
    connections: HashMap<String, ConnectedEntry>,
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

    pub fn reload(&mut self, configs: &[ConnectorConfig], force: bool) {
        let mut reusable = HashMap::new();
        if force {
            self.connections.clear();
        } else {
            let mut previous = std::mem::take(&mut self.connections);
            for cfg in configs.iter().filter(|cfg| cfg.enabled) {
                if let Some(mut entry) = previous.remove(&cfg.id) {
                    if entry.transport == cfg.transport && entry.connection.is_alive() {
                        reusable.insert(cfg.id.clone(), entry);
                    }
                }
            }
            drop(previous);
        }
        self.statuses.clear();
        self.tools.clear();

        for cfg in configs {
            if !cfg.enabled {
                self.statuses
                    .push(ConnectorStatus::from_cfg(cfg, false, None, &[]));
                continue;
            }

            if let Some(entry) = reusable.remove(&cfg.id) {
                self.tools.extend(entry.tools.iter().cloned());
                self.statuses
                    .push(ConnectorStatus::from_cfg(cfg, true, None, &entry.tools));
                self.connections.insert(cfg.id.clone(), entry);
                continue;
            }

            match connect_and_list(&self.sandbox, cfg) {
                Ok((connection, entry_tools)) => {
                    self.tools.extend(entry_tools.iter().cloned());
                    self.statuses
                        .push(ConnectorStatus::from_cfg(cfg, true, None, &entry_tools));
                    self.connections.insert(
                        cfg.id.clone(),
                        ConnectedEntry {
                            transport: cfg.transport.clone(),
                            connection,
                            tools: entry_tools,
                        },
                    );
                }
                Err(err) => {
                    self.statuses
                        .push(ConnectorStatus::from_cfg(cfg, false, Some(err), &[]));
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
        let Some(entry) = self.connections.get_mut(&tool.connector_id) else {
            return Outcome::Failed(format!("MCP 连接器未连接: {}", tool.connector_id));
        };
        let args = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        match entry.connection.request(
            "tools/call",
            json!({ "name": tool.name, "arguments": args }),
            TOOL_TIMEOUT,
        ) {
            Ok(result) => {
                if !result
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks
                            .iter()
                            .all(|block| match block.get("type").and_then(Value::as_str) {
                                Some("text") => block["text"].is_string(),
                                Some(kind) => !kind.is_empty(),
                                None => false,
                            })
                    })
                    || result.get("isError").is_some_and(|v| !v.is_boolean())
                {
                    return Outcome::Unknown("MCP 工具返回了无效结果".into());
                }
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

fn connect_and_list(
    sandbox: &Sandbox,
    cfg: &ConnectorConfig,
) -> Result<(ActiveConnection, Vec<McpTool>), String> {
    let mut conn = match &cfg.transport {
        ConnectorTransport::Stdio { command, args, env } => {
            StdioConnection::connect(sandbox, command, args, env).map(ActiveConnection::Stdio)?
        }
        ConnectorTransport::Http { url, headers } => {
            HttpConnection::connect(url, headers).map(ActiveConnection::Http)?
        }
    };
    let result = conn
        .request("tools/list", json!({}), MCP_TIMEOUT)
        .map_err(RequestError::into_message)?;
    let safe_id = sanitize_id(&cfg.id);
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.to_string();
            let description = item["description"].as_str().unwrap_or("").to_string();
            let input_schema = item
                .get("inputSchema")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let capability = format!("mcp.{safe_id}.{name}");
            Some(McpTool {
                connector_id: cfg.id.clone(),
                name,
                description,
                input_schema,
                capability,
            })
        })
        .collect();
    Ok((conn, tools))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_http_response_preserves_sse_blank_lines_and_handles_continue() {
        let raw = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nMcp-Session-Id: s1\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\r\n\r\n";
        let (headers, body) = split_http_response(raw).unwrap();
        assert!(headers.contains("Mcp-Session-Id: s1"));
        let parsed = parse_http_jsonrpc(&body, 1).unwrap();
        let result = extract_jsonrpc_result(&parsed, "err").unwrap();
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn split_http_response_distinguishes_unknown_gateway_and_explicit_errors() {
        let server_error = "HTTP/1.1 500 Internal Server Error\r\n\r\ncrashed after write";
        assert!(matches!(
            split_http_response(server_error),
            Err(RequestError::Unknown(ref msg)) if msg.contains("500")
        ));

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

        assert!(parse_http_jsonrpc(r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#, 1).is_err());
        assert!(
            parse_http_jsonrpc("data: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n", 1).is_err()
        );
        let missing_result = parse_http_jsonrpc(r#"{"jsonrpc":"2.0","id":1}"#, 1).unwrap();
        assert!(matches!(
            extract_jsonrpc_result(&missing_result, "err"),
            Err(RequestError::Unknown(_))
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
        if mode in ("null", "empty", "bad_flag", "bad_block"):
            result = {"null": None, "empty": {"content":[]}, "bad_flag":{"content":[], "isError":"yes"}, "bad_block":{"content":[None]}}[mode]
            print(json.dumps({"jsonrpc":"2.0","id":mid,"result":result}), flush=True)
        elif mode == "err":
            print(json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32000,"message":"rejected"}}), flush=True)
        else:
            sys.exit(0)
"#,
        )
        .unwrap();

        let mut pool = ConnectorPool::new(Sandbox::new(&core_dir).unwrap());
        pool.reload(
            &[ConnectorConfig {
                id: "flaky".into(),
                name: "Flaky".into(),
                enabled: true,
                transport: ConnectorTransport::Stdio {
                    command: "/usr/bin/python3".into(),
                    args: vec![script.display().to_string()],
                    env: BTreeMap::new(),
                },
            }],
            false,
        );

        for mode in ["null", "bad_flag", "bad_block"] {
            assert!(matches!(
                pool.call("mcp.flaky.op", &json!({"mode":mode})),
                Outcome::Unknown(_)
            ));
        }
        assert!(matches!(
            pool.call("mcp.flaky.op", &json!({"mode":"empty"})),
            Outcome::Observed(_)
        ));
        assert_eq!(
            pool.call("mcp.flaky.op", &json!({ "mode": "err" })),
            Outcome::Failed("rejected".into())
        );
        assert!(matches!(
            pool.call("mcp.flaky.op", &json!({ "mode": "drop" })),
            Outcome::Unknown(ref msg) if msg.contains("MCP 响应超时或进程退出")
        ));
    }

    #[test]
    fn incremental_reload_reuses_unchanged_connections_and_force_restarts_them() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path().join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        let script = dir.path().join("pid_mcp.py");
        std::fs::write(
            &script,
            r#"import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"pid","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"pid","description":"Pid","inputSchema":{"type":"object"}}]}}), flush=True)
    elif method == "tools/call":
        if msg.get("params", {}).get("arguments", {}).get("exit"):
            sys.exit(0)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":str(os.getpid())}]}}), flush=True)
"#,
        )
        .unwrap();

        let make_cfg = |id: &str, name: &str, enabled: bool| ConnectorConfig {
            id: id.into(),
            name: name.into(),
            enabled,
            transport: ConnectorTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            },
        };

        let mut pool = ConnectorPool::new(Sandbox::new(&core_dir).unwrap());
        let first = make_cfg("a", "A", true);
        let second = make_cfg("b", "B", true);
        pool.reload(std::slice::from_ref(&first), false);
        let Outcome::Observed(pid_before) = pool.call("mcp.a.pid", &json!({})) else {
            panic!("expected observed pid");
        };

        let mut renamed_first = first.clone();
        renamed_first.name = "A Renamed".into();
        pool.reload(&[renamed_first.clone(), second.clone()], false);
        assert_eq!(
            pool.call("mcp.a.pid", &json!({})),
            Outcome::Observed(pid_before.clone())
        );
        assert!(matches!(
            pool.call("mcp.b.pid", &json!({})),
            Outcome::Observed(_)
        ));

        pool.reload(std::slice::from_ref(&renamed_first), true);
        let Outcome::Observed(pid_after_force) = pool.call("mcp.a.pid", &json!({})) else {
            panic!("expected observed pid after force reload");
        };
        assert_ne!(pid_after_force, pid_before);

        assert!(matches!(
            pool.call("mcp.a.pid", &json!({ "exit": true })),
            Outcome::Unknown(_)
        ));
        pool.reload(std::slice::from_ref(&renamed_first), false);
        let Outcome::Observed(pid_after_crash) = pool.call("mcp.a.pid", &json!({})) else {
            panic!("expected observed pid after dead process reload");
        };
        assert_ne!(pid_after_crash, pid_after_force);
    }

    #[test]
    fn renaming_connector_id_closes_old_process_before_starting_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path().join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        let lock_file = dir.path().join("exclusive.lock");
        let script = dir.path().join("exclusive_mcp.py");
        std::fs::write(
            &script,
            r#"import fcntl, json, sys
lock = open(sys.argv[1], "w")
fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"lock","version":"1"}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object"}}]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"ok"}]}}), flush=True)
"#,
        )
        .unwrap();

        let make_cfg = |id: &str| ConnectorConfig {
            id: id.into(),
            name: id.into(),
            enabled: true,
            transport: ConnectorTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec![
                    script.display().to_string(),
                    lock_file.display().to_string(),
                ],
                env: BTreeMap::new(),
            },
        };

        let mut pool = ConnectorPool::new(Sandbox::new(&core_dir).unwrap());
        pool.reload(&[make_cfg("old_id")], false);
        assert_eq!(
            pool.call("mcp.old_id.ping", &json!({})),
            Outcome::Observed("ok".into())
        );

        pool.reload(&[make_cfg("new_id")], false);
        let statuses = pool.statuses();
        assert_eq!(statuses.len(), 1);
        assert!(
            statuses[0].connected,
            "expected new_id to connect: {:?}",
            statuses[0].error
        );
        assert_eq!(
            pool.call("mcp.new_id.ping", &json!({})),
            Outcome::Observed("ok".into())
        );
    }
}
