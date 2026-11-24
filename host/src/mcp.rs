use crate::config::{CapabilityConfig, CapabilityTransport};
use crate::hands::{execute_shell, run_process, ProcessInput, ToolExecution};
use crate::http::{post_bytes, truncate_utf8, HttpError, HttpResponse};
use crate::llm::{ModelImage, MAX_TOOL_IMAGE_BYTES};
use crate::oauth::{is_auth_error, OAuthStore, OAuthToken};
use crate::sandbox::{kill_process_tree, write_process_input, Sandbox, TrustedDir};
use base64::{engine::general_purpose::STANDARD, Engine};
use eja_core::Outcome;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{
    mpsc::{self, Receiver},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

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
pub struct CapabilityToolSummary {
    pub name: String,
    pub description: String,
    pub capability: String,
}

#[derive(Debug, Clone)]
pub struct CapabilityTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capability: String,
}

impl CapabilityTool {
    fn summary(&self) -> CapabilityToolSummary {
        CapabilityToolSummary {
            name: self.name.clone(),
            description: self.description.clone(),
            capability: self.capability.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityStatus {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auth_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools: Vec<CapabilityToolSummary>,
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
        self.send(&msg, Instant::now() + MCP_TIMEOUT)
    }

    fn send(&mut self, msg: &Value, deadline: Instant) -> Result<(), String> {
        let mut line = serde_json::to_vec(msg).map_err(|err| err.to_string())?;
        line.push(b'\n');
        write_process_input(&mut self.stdin, &line, deadline).map_err(|err| {
            self.broken = true;
            self.with_stderr(format!("写入 MCP 请求失败: {err}"))
        })
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
        let deadline = Instant::now() + timeout;
        self.send(&msg, deadline).map_err(RequestError::Unknown)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
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

    fn set_bearer_token(&mut self, token: &str) {
        self.headers
            .retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
        self.headers
            .insert("Authorization".to_string(), format!("Bearer {token}"));
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
    Builtin,
    Exec {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
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
            Self::Builtin | Self::Exec { .. } => Err(RequestError::Failed(
                "非 MCP 能力不支持 JSON-RPC 请求".into(),
            )),
        }
    }

    fn is_alive(&mut self) -> bool {
        match self {
            Self::Stdio(conn) => !conn.broken && matches!(conn.child.try_wait(), Ok(None)),
            Self::Builtin | Self::Exec { .. } | Self::Http(_) => true,
        }
    }
}

struct ConnectedCapability {
    connection: ActiveConnection,
    tools: Vec<CapabilityTool>,
    generation: u64,
}

struct CapabilityEntry {
    config: CapabilityConfig,
    oauth_code: Option<String>,
    connection: Result<Option<ConnectedCapability>, String>,
}

pub struct CapabilityPool {
    sandbox: Sandbox,
    oauth: OAuthStore,
    entries: Vec<CapabilityEntry>,
    next_generation: u64,
}

impl CapabilityPool {
    #[cfg(test)]
    pub fn new(sandbox: Sandbox) -> Self {
        Self::open(sandbox, None)
    }

    pub fn with_core_dir(sandbox: Sandbox, core_dir: TrustedDir) -> Self {
        Self::open(sandbox, Some(core_dir))
    }

    fn open(sandbox: Sandbox, core_dir: Option<TrustedDir>) -> Self {
        Self {
            sandbox,
            oauth: OAuthStore::load(core_dir),
            entries: Vec::new(),
            next_generation: 1,
        }
    }

    pub fn set_oauth_code(&mut self, id: &str, code: Option<String>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.config.id == id) {
            entry.oauth_code = code;
        }
    }

    pub fn save_oauth_token(&mut self, id: &str, token: OAuthToken) -> Result<(), String> {
        self.oauth.save(id, token)?;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.config.id == id) {
            entry.oauth_code = None;
            entry.connection = Ok(None);
        }
        Ok(())
    }

    pub fn register_local(&mut self, configs: &[CapabilityConfig]) {
        self.reconcile(configs, false, false);
    }

    pub fn reload(&mut self, configs: &[CapabilityConfig], force: bool) {
        self.reconcile(configs, force, true);
    }

    fn reconcile(&mut self, configs: &[CapabilityConfig], force: bool, connect_mcp: bool) {
        let mut previous: HashMap<_, _> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|entry| (entry.config.id.clone(), entry))
            .collect();
        previous.retain(|id, entry| {
            if !entry.config.enabled
                || !configs.iter().any(|config| {
                    config.enabled && config.id == *id && config.transport == entry.config.transport
                })
            {
                return false;
            }
            let connected = entry.connection.as_mut().ok().and_then(Option::as_mut);
            if force || connected.is_some_and(|conn| !conn.connection.is_alive()) {
                entry.connection = Ok(None);
            }
            true
        });

        for config in configs {
            let mut entry = previous
                .remove(&config.id)
                .unwrap_or_else(|| CapabilityEntry {
                    config: config.clone(),
                    oauth_code: None,
                    connection: Ok(None),
                });
            entry.config.clone_from(config);
            if config.enabled
                && (connect_mcp || !config.transport.is_mcp())
                && !matches!(entry.connection, Ok(Some(_)))
            {
                entry.connection = connect_and_list(&self.sandbox, &mut self.oauth, config).map(
                    |(connection, tools)| {
                        let generation = self.next_generation;
                        self.next_generation += 1;
                        Some(ConnectedCapability {
                            connection,
                            tools,
                            generation,
                        })
                    },
                );
            }
            self.entries.push(entry);
        }
    }

    pub fn disconnect_dead(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if let Ok(Some(active)) = &mut entry.connection {
                if !active.connection.is_alive() {
                    entry.connection = Err("MCP 连接已关闭".into());
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn statuses(&self) -> Vec<CapabilityStatus> {
        self.entries
            .iter()
            .map(|entry| {
                let active = entry.connection.as_ref().ok().and_then(Option::as_ref);
                let error = entry.connection.as_ref().err();
                CapabilityStatus {
                    id: entry.config.id.clone(),
                    name: entry.config.name.clone(),
                    enabled: entry.config.enabled,
                    connected: active.is_some(),
                    auth_required: matches!(
                        entry.config.transport,
                        CapabilityTransport::Http { .. }
                    ) && error.is_some_and(|err| is_auth_error(err)),
                    oauth_code: entry.oauth_code.clone(),
                    error: error.cloned(),
                    tools: active
                        .into_iter()
                        .flat_map(|active| &active.tools)
                        .map(CapabilityTool::summary)
                        .collect(),
                }
            })
            .collect()
    }

    pub fn tools(&self) -> impl Iterator<Item = (&CapabilityTool, u64)> {
        self.entries
            .iter()
            .filter_map(|entry| entry.connection.as_ref().ok()?.as_ref())
            .flat_map(|active| active.tools.iter().map(|tool| (tool, active.generation)))
    }

    pub fn is_current(&self, capability: &str, generation: u64) -> bool {
        self.tools()
            .any(|(tool, current)| current == generation && tool.capability == capability)
    }

    pub fn call(
        &mut self,
        capability: &str,
        call_id: eja_core::Id,
        input: &Value,
        data_dir: &TrustedDir,
    ) -> ToolExecution {
        let Some((config, connection, tool)) = self.entries.iter_mut().find_map(|entry| {
            let active = entry.connection.as_mut().ok()?.as_mut()?;
            let tool = active
                .tools
                .iter()
                .find(|tool| tool.capability == capability)?;
            Some((&entry.config, &mut active.connection, tool))
        }) else {
            return Outcome::Failed(format!("未知能力: {capability}")).into();
        };
        let args = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        match &connection {
            ActiveConnection::Builtin => {
                return execute_shell(&self.sandbox, data_dir, call_id, input).into();
            }
            ActiveConnection::Exec {
                command,
                args: cmd_args,
                env,
            } => {
                let stdin_bytes = serde_json::to_vec(&args).unwrap_or_else(|_| b"{}".to_vec());
                return run_process(
                    &self.sandbox,
                    command,
                    cmd_args,
                    env,
                    ProcessInput::Exec(&stdin_bytes),
                    data_dir.path(),
                    TOOL_TIMEOUT,
                    data_dir,
                    call_id,
                );
            }
            ActiveConnection::Stdio(_) | ActiveConnection::Http(_) => {}
        }
        let call_params = json!({ "name": tool.name, "arguments": args });
        let mut response = connection.request("tools/call", call_params.clone(), TOOL_TIMEOUT);
        if let Err(RequestError::Failed(ref err)) = response {
            if let CapabilityTransport::Http { headers, .. } = &config.transport {
                if is_auth_error(err) && !has_authorization_header(headers) {
                    if let ActiveConnection::Http(http_conn) = connection {
                        if let Some(new_token) = self.oauth.refresh(&config.id, &http_conn.url) {
                            http_conn.set_bearer_token(&new_token);
                            response = http_conn.request("tools/call", call_params, TOOL_TIMEOUT);
                        }
                    }
                }
            }
        }
        match response {
            Ok(result) => tool_result(&result),
            Err(RequestError::Failed(err)) => {
                Outcome::Failed(truncate_utf8(&err, MAX_OUTCOME_BYTES)).into()
            }
            Err(RequestError::Unknown(err)) => {
                Outcome::Unknown(truncate_utf8(&err, MAX_OUTCOME_BYTES)).into()
            }
        }
    }
}

fn connect_and_list(
    sandbox: &Sandbox,
    oauth: &mut OAuthStore,
    cfg: &CapabilityConfig,
) -> Result<(ActiveConnection, Vec<CapabilityTool>), String> {
    let mut conn = match &cfg.transport {
        CapabilityTransport::Builtin => {
            let tool = CapabilityTool {
                name: "shell".into(),
                description: "执行本机 shell 命令。".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "cwd": { "type": "string" },
                        "timeout_seconds": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["command"]
                }),
                capability: "shell".into(),
            };
            return Ok((ActiveConnection::Builtin, vec![tool]));
        }
        CapabilityTransport::Exec {
            command,
            args,
            env,
            description,
            schema,
        } => {
            let trimmed = command.trim();
            if trimmed.is_empty() {
                return Err("未配置执行命令".into());
            }
            let id = route_component(&cfg.id);
            let desc = if description.trim().is_empty() {
                cfg.name.clone()
            } else {
                description.clone()
            };
            let input_schema = schema
                .clone()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let tool = CapabilityTool {
                name: cfg.id.clone(),
                description: desc,
                input_schema,
                capability: format!("cap.{id}"),
            };
            return Ok((
                ActiveConnection::Exec {
                    command: trimmed.to_string(),
                    args: args.clone(),
                    env: env.clone(),
                },
                vec![tool],
            ));
        }
        CapabilityTransport::Stdio { command, args, env } => {
            StdioConnection::connect(sandbox, command, args, env).map(ActiveConnection::Stdio)?
        }
        CapabilityTransport::Http { url, headers } => {
            let mut effective = headers.clone();
            let manual_auth = has_authorization_header(&effective);
            if !manual_auth {
                if let Some(token) = oauth.access_token(&cfg.id, url) {
                    effective.insert("Authorization".to_string(), format!("Bearer {token}"));
                }
            }
            match HttpConnection::connect(url, &effective) {
                Ok(http) => ActiveConnection::Http(http),
                Err(err) if !manual_auth && is_auth_error(&err) => {
                    if let Some(refreshed) = oauth.refresh(&cfg.id, url) {
                        effective
                            .insert("Authorization".to_string(), format!("Bearer {refreshed}"));
                        HttpConnection::connect(url, &effective).map(ActiveConnection::Http)?
                    } else {
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            }
        }
    };
    let result = conn
        .request("tools/list", json!({}), MCP_TIMEOUT)
        .map_err(RequestError::into_message)?;
    let id = route_component(&cfg.id);
    let mut names = HashSet::new();
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or("MCP 工具列表无效")?
        .iter()
        .map(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("MCP 工具名称无效")?;
            if !names.insert(name) {
                return Err("MCP 工具名称重复");
            }
            let description = item["description"].as_str().unwrap_or("").to_string();
            let input_schema = item
                .get("inputSchema")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let capability = format!("cap.{id}.{}", route_component(name));
            Ok(CapabilityTool {
                name: name.into(),
                description,
                input_schema,
                capability,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((conn, tools))
}

fn tool_result(result: &Value) -> ToolExecution {
    let invalid = || Outcome::Unknown("MCP 工具返回了无效结果".into()).into();
    let Some(blocks) = result.get("content").and_then(Value::as_array) else {
        return invalid();
    };
    if result
        .get("isError")
        .is_some_and(|value| !value.is_boolean())
    {
        return invalid();
    }
    let mut parts = Vec::new();
    let mut images = Vec::new();
    let mut image_bytes = MAX_TOOL_IMAGE_BYTES;
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let Some(text) = block["text"].as_str() else {
                    return invalid();
                };
                parts.push(text.to_string());
            }
            Some("image") => {
                let image = block["data"]
                    .as_str()
                    .filter(|data| data.len() <= image_bytes.div_ceil(3) * 4)
                    .and_then(|data| STANDARD.decode(data).ok())
                    .filter(|bytes| bytes.len() <= image_bytes)
                    .and_then(|bytes| {
                        let media_type = ModelImage::media_type(&bytes)?;
                        (block["mimeType"].as_str() == Some(media_type))
                            .then_some(ModelImage { media_type, bytes })
                    });
                if let Some(image) = image {
                    image_bytes -= image.bytes.len();
                    parts.push(image.receipt());
                    images.push(image);
                } else {
                    parts.push("MCP 图像格式无效或超出大小限制".into());
                }
            }
            Some("resource") => {
                let resource = &block["resource"];
                parts.push(
                    resource["text"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("工具返回资源 {}", resource["uri"].as_str().unwrap_or(""))
                        }),
                );
            }
            Some("resource_link") => parts.push(format!(
                "工具返回资源链接 {}",
                block["uri"].as_str().unwrap_or("")
            )),
            Some(kind) if !kind.is_empty() => parts.push(format!("工具返回 {kind} 内容")),
            _ => return invalid(),
        }
    }
    if parts.is_empty() {
        parts.push("(no output)".into());
    }
    let text = truncate_utf8(&parts.join("\n"), MAX_OUTCOME_BYTES);
    let outcome = if result["isError"].as_bool() == Some(true) {
        Outcome::Failed(text)
    } else {
        Outcome::Observed(text)
    };
    ToolExecution { outcome, images }
}

fn has_authorization_header(headers: &BTreeMap<String, String>) -> bool {
    headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && !v.trim().is_empty())
}

fn route_component(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_keeps_raw_ids_distinct_and_mcp_images_stay_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let data = TrustedDir::open(dir.path()).unwrap();
        let config = |id: &str, text: &str| CapabilityConfig {
            id: id.into(),
            name: id.into(),
            enabled: true,
            transport: CapabilityTransport::Exec {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), format!("cat >/dev/null; printf %s {text}")],
                env: BTreeMap::new(),
                description: String::new(),
                schema: None,
            },
        };
        let core = dir.path().join("private");
        std::fs::create_dir(&core).unwrap();
        let mut pool = CapabilityPool::new(Sandbox::new(&core).unwrap());
        pool.reload(&[config("a.b", "first"), config("a_b", "second")], false);
        let routes: Vec<_> = pool
            .tools()
            .map(|(tool, _)| tool.capability.clone())
            .collect();
        assert_ne!(routes[0], routes[1]);
        assert_eq!(
            pool.call(&routes[1], 1, &json!({}), &data).outcome,
            Outcome::Observed("second".into())
        );
        assert_ne!(route_component("a.b"), route_component("a%2Eb"));

        let picture = b"\x89PNG\r\n\x1a\n";
        let encoded = STANDARD.encode(picture);
        let result = tool_result(&json!({"content":[
            {"type":"text","text":"完整回执"},
            {"type":"image","mimeType":"image/png","data":encoded},
        ]}));
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].bytes, picture);
        assert!(matches!(result.outcome, Outcome::Observed(ref text)
            if text.contains("完整回执") && !text.contains(&encoded)));
    }

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

        let data_dir = TrustedDir::open(dir.path()).unwrap();
        let mut pool = CapabilityPool::new(Sandbox::new(&core_dir).unwrap());
        pool.reload(
            &[CapabilityConfig {
                id: "flaky".into(),
                name: "Flaky".into(),
                enabled: true,
                transport: CapabilityTransport::Stdio {
                    command: "/usr/bin/python3".into(),
                    args: vec![script.display().to_string()],
                    env: BTreeMap::new(),
                },
            }],
            false,
        );

        for mode in ["null", "bad_flag", "bad_block"] {
            assert!(matches!(
                pool.call("cap.flaky.op", 1, &json!({"mode":mode}), &data_dir)
                    .outcome,
                Outcome::Unknown(_)
            ));
        }
        assert!(matches!(
            pool.call("cap.flaky.op", 1, &json!({"mode":"empty"}), &data_dir)
                .outcome,
            Outcome::Observed(_)
        ));
        assert_eq!(
            pool.call("cap.flaky.op", 1, &json!({ "mode": "err" }), &data_dir)
                .outcome,
            Outcome::Failed("rejected".into())
        );
        assert!(matches!(
            pool.call("cap.flaky.op", 1, &json!({ "mode": "drop" }), &data_dir).outcome,
            Outcome::Unknown(ref msg) if msg.contains("MCP 响应超时或进程退出")
        ));
    }

    #[test]
    fn incremental_reload_reuses_unchanged_connections_and_force_restarts_them() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = TrustedDir::open(dir.path()).unwrap();
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

        let make_cfg = |id: &str, name: &str, enabled: bool| CapabilityConfig {
            id: id.into(),
            name: name.into(),
            enabled,
            transport: CapabilityTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            },
        };

        let mut pool = CapabilityPool::new(Sandbox::new(&core_dir).unwrap());
        let first = make_cfg("a", "A", true);
        let second = make_cfg("b", "B", true);
        pool.reload(std::slice::from_ref(&first), false);
        let Outcome::Observed(pid_before) =
            pool.call("cap.a.pid", 1, &json!({}), &data_dir).outcome
        else {
            panic!("expected observed pid");
        };

        let mut renamed_first = first.clone();
        renamed_first.name = "A Renamed".into();
        pool.reload(&[renamed_first.clone(), second.clone()], false);
        assert_eq!(
            pool.call("cap.a.pid", 1, &json!({}), &data_dir).outcome,
            Outcome::Observed(pid_before.clone())
        );
        assert!(matches!(
            pool.call("cap.b.pid", 1, &json!({}), &data_dir).outcome,
            Outcome::Observed(_)
        ));

        pool.reload(std::slice::from_ref(&renamed_first), true);
        let Outcome::Observed(pid_after_force) =
            pool.call("cap.a.pid", 1, &json!({}), &data_dir).outcome
        else {
            panic!("expected observed pid after force reload");
        };
        assert_ne!(pid_after_force, pid_before);

        assert!(matches!(
            pool.call("cap.a.pid", 1, &json!({ "exit": true }), &data_dir)
                .outcome,
            Outcome::Unknown(_)
        ));
        pool.reload(std::slice::from_ref(&renamed_first), false);
        let Outcome::Observed(pid_after_crash) =
            pool.call("cap.a.pid", 1, &json!({}), &data_dir).outcome
        else {
            panic!("expected observed pid after dead process reload");
        };
        assert_ne!(pid_after_crash, pid_after_force);
    }

    #[test]
    fn renaming_capability_id_closes_old_process_before_starting_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = TrustedDir::open(dir.path()).unwrap();
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

        let make_cfg = |id: &str| CapabilityConfig {
            id: id.into(),
            name: id.into(),
            enabled: true,
            transport: CapabilityTransport::Stdio {
                command: "/usr/bin/python3".into(),
                args: vec![
                    script.display().to_string(),
                    lock_file.display().to_string(),
                ],
                env: BTreeMap::new(),
            },
        };

        let mut pool = CapabilityPool::new(Sandbox::new(&core_dir).unwrap());
        pool.reload(&[make_cfg("old_id")], false);
        assert_eq!(
            pool.call("cap.old_id.ping", 1, &json!({}), &data_dir)
                .outcome,
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
            pool.call("cap.new_id.ping", 1, &json!({}), &data_dir)
                .outcome,
            Outcome::Observed("ok".into())
        );
    }
}
