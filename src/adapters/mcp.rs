//! Shared, host-owned MCP clients. No connector state is passed to AJA.
use super::Capabilities;
use super::{Control, FileTools, ToolContext, ToolGateway};
use crate::{aja::FRAME_LIMIT, Assignment, Result};
use rmcp::{
    model::{
        CallToolRequest, CallToolRequestParams, ClientCapabilities, ClientConfig, ClientRequest,
        Implementation, PaginatedRequestParams, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RunningService},
    transport::{
        async_rw::AsyncRwTransport, streamable_http_client::StreamableHttpClientTransportConfig,
        StreamableHttpClientTransport,
    },
    RoleClient, ServiceExt,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    runtime::Runtime,
};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    command: Option<String>,
    url: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    tools: Option<Vec<String>>,
    /// Server tool names the host declares read-only for EJA's observation.
    #[serde(default)]
    read_only: Vec<String>,
    /// Server tool names that may answer an acceptance witness. A witness is an
    /// effect-capable host act, so it is declared here and nowhere else.
    #[serde(default)]
    witness: Vec<String>,
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default = "timeout")]
    timeout_ms: u64,
}
const fn enabled() -> bool {
    true
}
const fn timeout() -> u64 {
    30_000
}

/// How long a cancelled request may take to acknowledge the cancellation.
const CANCEL_TIMEOUT: Duration = Duration::from_millis(500);

fn expand(value: &str) -> Result<String> {
    let mut result = String::new();
    let mut rest = value;
    while let Some(at) = rest.find("${") {
        result.push_str(&rest[..at]);
        let tail = &rest[at + 2..];
        let end = tail
            .find('}')
            .ok_or("unterminated MCP environment reference")?;
        let name = &tail[..end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err("invalid MCP environment reference".into());
        }
        result.push_str(
            &std::env::var(name).map_err(|_| format!("missing MCP environment variable {name}"))?,
        );
        rest = &tail[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

fn alias(server: &str, tool: &str) -> String {
    let normalized: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("mcp__{server}__{normalized}");
    if name.len() <= 64 {
        return name;
    }
    // Stable short suffix for long tool names. Collisions are rejected below.
    let hash = tool.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{}_{hash:016x}", &name[..47])
}

struct LocalServer {
    child: Child,
    stopped: bool,
}
impl LocalServer {
    fn kill(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Some(id) = self.child.id() {
            // A child id is a process id; one that cannot be a group id is not a
            // group to signal, and the guard still closes the child's pipes.
            if let Ok(group) = libc::pid_t::try_from(id) {
                // SAFETY: the server was spawned into its own process group, so its
                // id is a live group id owned by this process; killing that group
                // ends the server and anything it started. A reaped group is a
                // no-op error.
                unsafe {
                    libc::killpg(group, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.start_kill();
    }
    async fn close(&mut self) {
        self.kill();
        let _ = self.child.wait().await;
    }
}
impl Drop for LocalServer {
    fn drop(&mut self) {
        self.kill();
    }
}

struct RemoteTool {
    server: usize,
    name: String,
    definition: Value,
    read_only: bool,
    /// The host declared this tool may answer an acceptance witness.
    witness: bool,
}
struct Connection {
    name: String,
    timeout: Duration,
    service: RunningService<RoleClient, ClientConfig>,
}

/// One registry/runtime for the application, shared across disposable ajas.
pub struct McpTools {
    runtime: Runtime,
    connections: Vec<Connection>,
    children: Vec<LocalServer>,
    tools: BTreeMap<String, RemoteTool>,
}

impl McpTools {
    /// A tool is observable when the server says so or the host declares it.
    pub fn is_read_only(&self, public: &str) -> bool {
        self.tools
            .get(public)
            .is_some_and(|remote| remote.read_only)
    }

    /// A tool may answer a witness only when the host declared it.
    pub(crate) fn is_witness(&self, public: &str) -> bool {
        self.tools.get(public).is_some_and(|remote| remote.witness)
    }

    /// Explicit host config only; no home/project auto-discovery or shell expansion.
    pub fn load(path: &Path, project: &Path, parent: &Control) -> Result<Self> {
        parent.check()?;
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(FRAME_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > FRAME_LIMIT {
            return Err("MCP config exceeds limit".into());
        }
        let config: Config = serde_json::from_slice(&bytes)?;
        if config.servers.len() > 32 {
            return Err("MCP config exceeds 32 servers".into());
        }
        let mut host = Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
            connections: vec![],
            children: vec![],
            tools: BTreeMap::new(),
        };
        let startup = parent.child(Duration::from_secs(300));
        for (name, config) in config.servers {
            startup.check()?;
            if !config.enabled {
                continue;
            }
            if name.is_empty()
                || name.len() > 24
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(
                    "MCP server names must be 1..24 ASCII letters, digits, underscores or hyphens"
                        .into(),
                );
            }
            if config.timeout_ms == 0 || config.timeout_ms > 300_000 {
                return Err(format!("invalid MCP timeout for {name}").into());
            }
            let control = startup.child(Duration::from_millis(config.timeout_ms));
            let timeout = Duration::from_millis(config.timeout_ms);
            let client = ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("eja", env!("CARGO_PKG_VERSION")),
            );
            let service = match (&config.command, &config.url) {
                (Some(program), None) => {
                    if !config.headers.is_empty() {
                        return Err("stdio MCP cannot have HTTP headers".into());
                    }
                    let cwd = config.cwd.as_ref().map_or_else(
                        || project.to_path_buf(),
                        |p| {
                            if p.is_relative() {
                                path.parent().unwrap_or_else(|| Path::new(".")).join(p)
                            } else {
                                p.clone()
                            }
                        },
                    );
                    let mut command = Command::new(program);
                    command
                        .args(&config.args)
                        .current_dir(cwd)
                        .env_clear()
                        .kill_on_drop(true)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::inherit());
                    for key in ["PATH", "HOME", "TMPDIR", "TMP", "LANG", "SYSTEMROOT"] {
                        if let Some(value) = std::env::var_os(key) {
                            command.env(key, value);
                        }
                    }
                    for (key, value) in &config.env {
                        command.env(key, expand(value)?);
                    }
                    command.process_group(0);
                    let mut child = host.runtime.block_on(async { command.spawn() })?;
                    let output = child.stdout.take().ok_or("missing MCP stdout")?;
                    let input = child.stdin.take().ok_or("missing MCP stdin")?;
                    host.children.push(LocalServer {
                        child,
                        stopped: false,
                    });
                    let transport = AsyncRwTransport::new_client(output, input)
                        .with_max_line_length(FRAME_LIMIT * 2);
                    host.runtime.block_on(async {
                        super::controlled(&control, async {
                            Ok(client
                                .serve(transport)
                                .await
                                .map_err(|_| format!("MCP {name}: stdio initialization failed"))?)
                        })
                        .await
                    })?
                }
                (None, Some(url)) => {
                    if !config.args.is_empty() || !config.env.is_empty() || config.cwd.is_some() {
                        return Err("HTTP MCP cannot have command arguments, env or cwd".into());
                    }
                    let parsed = reqwest::Url::parse(url)?;
                    if !matches!(parsed.scheme(), "http" | "https")
                        || !parsed.username().is_empty()
                        || parsed.password().is_some()
                    {
                        return Err(
                            "MCP requires an HTTP(S) endpoint without URL credentials".into()
                        );
                    }
                    let mut headers = HashMap::new();
                    for (key, value) in &config.headers {
                        let key = reqwest::header::HeaderName::from_bytes(key.as_bytes())?;
                        if matches!(
                            key.as_str(),
                            "mcp-session-id"
                                | "mcp-protocol-version"
                                | "content-type"
                                | "accept"
                                | "host"
                                | "content-length"
                        ) {
                            return Err("MCP protocol headers are managed by the host".into());
                        }
                        headers.insert(
                            key,
                            reqwest::header::HeaderValue::from_str(&expand(value)?)?,
                        );
                    }
                    let http = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(timeout)
                        .build()?;
                    host.runtime.block_on(async {
                        let transport = StreamableHttpClientTransport::with_client(
                            http,
                            StreamableHttpClientTransportConfig::with_uri(url.clone())
                                .custom_headers(headers)
                                .max_sse_event_size(FRAME_LIMIT * 2)
                                .max_concurrent_requests(1)
                                .control_request_timeout(Duration::from_millis(500))
                                .reinit_on_expired_session(true),
                        );
                        super::controlled(&control, async {
                            Ok(client
                                .serve(transport)
                                .await
                                .map_err(|_| format!("MCP {name}: HTTP initialization failed"))?)
                        })
                        .await
                    })?
                }
                _ => return Err(format!("MCP {name} needs exactly one command or url").into()),
            };
            let index = host.connections.len();
            host.connections.push(Connection {
                name: name.clone(),
                timeout,
                service,
            });
            let listed = host.runtime.block_on(async {
                super::controlled(&control, async {
                    let mut all = Vec::new();
                    let mut cursor = None;
                    let mut seen = HashSet::new();
                    let mut bytes = 0;
                    loop {
                        let page = host.connections[index]
                            .service
                            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                            .await?;
                        bytes += serde_json::to_vec(&page.tools)?.len();
                        all.extend(page.tools);
                        if all.len() > 256 || seen.len() >= 256 {
                            return Err("MCP catalog exceeds 256 tools/pages".into());
                        }
                        if bytes > FRAME_LIMIT {
                            return Err("MCP tool catalog exceeds limit".into());
                        }
                        cursor = page.next_cursor;
                        match &cursor {
                            None => return Ok::<Vec<Tool>, crate::Error>(all),
                            Some(c) if !seen.insert(c.clone()) => {
                                return Err("MCP pagination cursor repeated".into())
                            }
                            _ => {}
                        }
                    }
                })
                .await
            })?;
            let requested: Option<HashSet<_>> = config
                .tools
                .as_ref()
                .map(|items| items.iter().cloned().collect());
            let available: HashSet<_> = listed.iter().map(|t| t.name.to_string()).collect();
            if requested
                .as_ref()
                .is_some_and(|wanted| !wanted.is_subset(&available))
            {
                return Err(format!(
                    "MCP {name}: configured tool is absent from the server catalog"
                )
                .into());
            }
            if config
                .read_only
                .iter()
                .any(|declared| !available.contains(declared))
            {
                return Err(format!(
                    "MCP {name}: read_only tool is absent from the server catalog"
                )
                .into());
            }
            if config
                .witness
                .iter()
                .any(|declared| !available.contains(declared))
            {
                return Err(
                    format!("MCP {name}: witness tool is absent from the server catalog").into(),
                );
            }
            for tool in listed {
                if requested
                    .as_ref()
                    .is_some_and(|wanted| !wanted.contains(tool.name.as_ref()))
                {
                    continue;
                }
                let public = alias(&name, &tool.name);
                let definition = json!({
                    "type": "function",
                    "function": {
                        "name": public,
                        "description": format!("MCP {name}/{}: {}", tool.name, tool.description.as_deref().unwrap_or("")),
                        "parameters": tool.input_schema
                    }
                });
                if host
                    .tools
                    .insert(
                        public,
                        RemoteTool {
                            server: index,
                            name: tool.name.to_string(),
                            definition,
                            witness: config
                                .witness
                                .iter()
                                .any(|witness| witness == tool.name.as_ref()),
                            read_only: config
                                .read_only
                                .iter()
                                .any(|name| name == tool.name.as_ref())
                                || tool
                                    .annotations
                                    .as_ref()
                                    .and_then(|annotations| annotations.read_only_hint)
                                    .unwrap_or(false),
                        },
                    )
                    .is_some()
                {
                    return Err("MCP tool names collide after normalization".into());
                }
            }
            if serde_json::to_vec(
                &host
                    .tools
                    .values()
                    .map(|t| &t.definition)
                    .collect::<Vec<_>>(),
            )?
            .len()
                > FRAME_LIMIT
            {
                return Err("MCP tool definitions exceed limit".into());
            }
        }
        Ok(host)
    }
}

impl ToolGateway for McpTools {
    fn definitions(&self, _: &Assignment) -> Vec<Value> {
        self.tools.values().map(|t| t.definition.clone()).collect()
    }
    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        ctx.control.check()?;
        let tool = self.tools.get(name).ok_or("MCP tool not granted")?;
        let connection = &self.connections[tool.server];
        let args = args
            .as_object()
            .ok_or("MCP arguments must be an object")?
            .clone();
        let remaining = ctx.control.remaining()?.min(connection.timeout);
        let peer = connection.service.peer().clone();
        let result = self
            .runtime
            .block_on(async {
                let request = ClientRequest::CallToolRequest(CallToolRequest::new(
                    CallToolRequestParams::new(tool.name.clone()).with_arguments(args),
                ));
                let call_control = ctx.control.child(remaining);
                let mut handle = super::controlled(&call_control, async {
                    Ok(peer
                        .send_cancellable_request(
                            request,
                            PeerRequestOptions::with_timeout(remaining),
                        )
                        .await
                        .map_err(|error| error.to_string())?)
                })
                .await?;
                let cancel_deadline = tokio::time::Instant::now() + call_control.remaining()?;
                let response = tokio::select! {
                    biased;
                    response = &mut handle.rx => Some(response??),
                    () = tokio::time::sleep_until(cancel_deadline) => None,
                    () = super::cancelled(&call_control) => None,
                };
                // A stop asks the server to stop rather than dropping the call.
                let Some(response) = response else {
                    let error = super::stop_reason(&call_control);
                    let _ = tokio::time::timeout(
                        CANCEL_TIMEOUT,
                        handle.cancel(Some(error.to_string())),
                    )
                    .await;
                    return Err::<ServerResult, crate::Error>(error);
                };
                Ok::<ServerResult, crate::Error>(response)
            })
            .map_err(|e| format!("MCP {}: {e}", connection.name))?;
        ctx.control.check()?;
        let ServerResult::CallToolResult(result) = result else {
            return Err("MCP tool requires an unsupported interactive or task continuation".into());
        };
        let value = serde_json::to_value(result)?;
        if crate::measure::bytes(&value)? > crate::aja::FRAME_LIMIT {
            return Err("MCP tool output exceeds limit".into());
        }
        Ok(value)
    }
}

impl Drop for McpTools {
    fn drop(&mut self) {
        self.runtime.block_on(async {
            for connection in &mut self.connections {
                let _ = connection
                    .service
                    .close_with_timeout(Duration::from_millis(500))
                    .await;
            }
            for child in &mut self.children {
                child.close().await;
            }
        });
    }
}

/// The application's one tool entry point. Shared MCP servers outlive ajas.
pub struct HostTools {
    mcp: Option<McpTools>,
    project: PathBuf,
    private: PathBuf,
    workspace_tools: bool,
    roots: Vec<PathBuf>,
}
impl HostTools {
    pub fn new(
        config: Option<&Path>,
        project: &Path,
        private: &Path,
        control: &Control,
    ) -> Result<Self> {
        Ok(Self {
            mcp: config
                .map(|p| McpTools::load(p, project, control))
                .transpose()?,
            project: project.canonicalize()?,
            private: private.canonicalize()?,
            workspace_tools: true,
            roots: Vec::new(),
        })
    }

    /// The roots the host grants this deployment. The host's own reads reach
    /// every granted workspace; a run still writes only its own world.
    #[must_use]
    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        self
    }

    fn read(&self, tool: &str, arguments: &Value, control: &Control) -> Result<Value> {
        let path = arguments["path"].as_str().ok_or("missing path")?;
        let roots: Vec<PathBuf> = if self.roots.is_empty() {
            vec![self.project.clone()]
        } else {
            self.roots.clone()
        };
        let full = super::granted_path(&roots, path)?;
        if full.starts_with(&self.private) {
            return Err("the host's private state is not readable".into());
        }
        let root = roots
            .iter()
            .find(|root| full.starts_with(root))
            .ok_or("path is outside every granted workspace")?;
        let context = ToolContext {
            aja_id: 0,
            project: root,
            private: &self.private,
            control,
        };
        // The resolved path is the answer: re-resolving the caller's relative
        // name against the first matching root could address another grant.
        let resolved = json!({"path": full.to_string_lossy()});
        FileTools.call(&context, tool, &resolved)
    }

    /// The built-in workspace file tools are a grant the host may switch off;
    /// connectors are unaffected.
    #[must_use]
    pub const fn with_workspace_tools(mut self, enabled: bool) -> Self {
        self.workspace_tools = enabled;
        self
    }
}
impl ToolGateway for HostTools {
    /// A aja works its granted workspace through the built-in file tools, and
    /// reaches anything beyond it only through the host's declared connectors.
    /// The file tools resolve every path inside the run's world and refuse the
    /// host's private state, so the grant cannot widen the workspace.
    fn definitions(&self, assignment: &Assignment) -> Vec<Value> {
        let mut definitions = if self.workspace_tools {
            FileTools.definitions(assignment)
        } else {
            Vec::new()
        };
        definitions.extend(
            self.mcp
                .as_ref()
                .map_or_else(Vec::new, |mcp| mcp.definitions(assignment)),
        );
        definitions
    }
    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        if matches!(
            name,
            "read_file" | "list_dir" | "edit_file" | "write_file" | "glob" | "grep" | "shell"
        ) {
            if !self.workspace_tools {
                return Err("the workspace file tools are switched off".into());
            }
            return FileTools.call(ctx, name, args);
        }
        if !name.starts_with("mcp__") {
            return Err(format!("{name} is not a declared connector tool").into());
        }
        self.mcp
            .as_ref()
            .ok_or("MCP is not configured")?
            .call(ctx, name, args)
    }
}

impl Capabilities for HostTools {
    /// EJA may look at the world; it may never act. Files are readable only from
    /// the granted workspaces, connector tools only when declared read-only.
    fn observe(&self, tool: &str, arguments: &Value, parent: &Control) -> Result<Value> {
        let control = parent.child(Duration::from_secs(60));
        if tool == "read_file" || tool == "list_dir" {
            if !self.workspace_tools {
                return Err("the workspace file tools are switched off".into());
            }
            return self.read(tool, arguments, &control);
        }
        if tool == "write_file" || tool == "edit_file" {
            return Err("EJA only observes; edits belong to ajas".into());
        }
        let context = ToolContext {
            aja_id: 0,
            project: &self.project,
            private: &self.private,
            control: &control,
        };
        let mcp = self.mcp.as_ref().ok_or("MCP is not configured")?;
        if !mcp.is_read_only(tool) {
            return Err("EJA may only observe read-only connector tools".into());
        }
        mcp.call(&context, tool, arguments)
    }

    /// A witness is one declared host capability. The request names the tool; the
    /// host decides whether that name may answer it, and reading a file can have
    /// no effect, so the host's own read tools answer while the grant is on.
    fn witness(&self, request: &Value, control: &Control) -> Result<Value> {
        let tool = request["tool"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("a witness names one host tool")?;
        let arguments = request
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let control = control.child(Duration::from_secs(120));
        control.check()?;
        if tool == "read_file" || tool == "list_dir" {
            if !self.workspace_tools {
                return Err("the workspace file tools are switched off".into());
            }
            return self.read(tool, &arguments, &control);
        }
        let context = ToolContext {
            aja_id: 0,
            project: &self.project,
            private: &self.private,
            control: &control,
        };
        let mcp = self.mcp.as_ref().ok_or("MCP is not configured")?;
        if !mcp.is_witness(tool) {
            return Err(format!("{tool} is not a declared witness").into());
        }
        mcp.call(&context, tool, &arguments)
    }
}
