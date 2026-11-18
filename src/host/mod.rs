mod plugin_sandbox;
mod plugins;
mod settings;
mod state;
mod util;

pub use settings::SettingsStore;

use self::{
    settings::{connector_config, RoleConfig, StoredConnector, StoredSettings},
    util::{clip, safe_file, ApiError, ApiResult},
};
use axum::{
    body::to_bytes,
    extract::{Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use eja::{
    adapters::{
        Control, HostTools, NativeWorker, OpenAiBrain, OpenAiModel, Ports, RecordFiles, Shared,
        ToolGateway, Worktrees, Worlds,
    },
    turn_at_with_options, Assignment, ChoiceSelection, Kind, Store, TurnOptions,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_TEXT: usize = 200_000;
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const MAX_RAW: usize = 128 * 1024;

/// The one place the rhythm is defined. It is policy, not a second source of
/// time: how often the host looks, and how much quiet opens an `alone`
/// situation. What that quiet means is decided by EJA, not here. The facts it
/// compares are the journal's own `at` stamps, sampled from the OS clock once
/// per turn; the host keeps no clock of its own.
struct Pulse {
    tick: Duration,
    quiet: i64,
}

const PULSE: Pulse = Pulse {
    tick: Duration::from_secs(60),
    quiet: 1800,
};
const PROVIDER_TIMEOUT_MS: u64 = 15_000;
const CONNECTOR_TEST_LIMIT_MS: u64 = 30_000;

struct TurnFlag<'a>(&'a AtomicBool);

impl Drop for TurnFlag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone)]
pub struct Options {
    pub state_dir: PathBuf,
    pub project_dir: Option<PathBuf>,
    pub host: String,
    pub port: u16,
    pub allowed_origins: Vec<String>,
    pub capabilities: EnabledCapabilities,
}

#[derive(Clone, Copy, Debug)]
pub enum Capability {
    Workspace,
    Files,
    Shell,
    Git,
    Mcp,
    Plugins,
}

#[derive(Clone, Copy, Debug)]
pub struct EnabledCapabilities([bool; 6]);

impl EnabledCapabilities {
    pub fn from_env() -> eja::Result<Self> {
        let configured = match std::env::var("EJA_CAPABILITIES") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => "workspace,files,shell,git,mcp".into(),
            Err(error) => return Err(error.into()),
        };
        let mut enabled = Self([false; 6]);
        for item in configured
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let capability = match item {
                "workspace" => Capability::Workspace,
                "files" => Capability::Files,
                "shell" => Capability::Shell,
                "git" => Capability::Git,
                "mcp" => Capability::Mcp,
                "plugins" => Capability::Plugins,
                _ => return Err(format!("unknown EJA_CAPABILITIES entry {item}").into()),
            };
            enabled.0[capability as usize] = true;
        }
        Ok(enabled)
    }

    pub const fn has(self, capability: Capability) -> bool {
        self.0[capability as usize]
    }

    pub fn grants(
        self,
        settings: &SettingsStore,
        stored: &StoredSettings,
    ) -> Vec<eja::WorkspaceGrant> {
        if self.has(Capability::Workspace) {
            settings.grant(stored)
        } else {
            Vec::new()
        }
    }

    pub const fn with_workspace_tools(mut self, enabled: bool) -> Self {
        self.0[Capability::Files as usize] &= enabled;
        self.0[Capability::Shell as usize] &= enabled;
        self
    }
}

pub struct Host {
    state_dir: PathBuf,
    project_dir: Option<PathBuf>,
    settings: SettingsStore,
    allowed_origins: Vec<String>,
    capabilities: EnabledCapabilities,
    turn_in_flight: AtomicBool,
    cancelled: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct TurnRequest {
    #[serde(default)]
    text: String,
    #[serde(default)]
    selection: Option<ChoiceSelection>,
    #[serde(default)]
    focus: Option<u64>,
    #[serde(default)]
    approvals: Vec<u64>,
    #[serde(default)]
    denials: Vec<u64>,
    #[serde(default)]
    gesture: Option<String>,
}

#[derive(Deserialize)]
struct RawQuery {
    #[serde(default)]
    address: String,
}

/// The state query: a view's local busy flag and encounter bookmark. Without
/// an anchor the loops are the return screen's; with one, the encounter's.
#[derive(Deserialize, Default)]
struct StateQuery {
    #[serde(default)]
    busy: String,
    anchor: Option<u64>,
    #[serde(rename = "taskId")]
    task_id: Option<u64>,
    #[serde(rename = "proposalId")]
    proposal_id: Option<u64>,
}

pub fn serve(options: Options) -> eja::Result<()> {
    if options.port == 0 {
        return Err("--port must be between 1 and 65535".into());
    }
    if options.capabilities.has(Capability::Plugins)
        && !(options.host == "localhost"
            || options
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback()))
    {
        return Err("managed plugin administration requires a local runtime address".into());
    }
    let project_dir = options
        .project_dir
        .filter(|_| options.capabilities.has(Capability::Workspace))
        .map(|path| {
            let path = path
                .canonicalize()
                .map_err(|error| format!("project directory is unavailable: {error}"))?;
            if !path.is_dir() {
                return Err(format!(
                    "project directory does not exist: {}",
                    path.display()
                ));
            }
            Ok(path)
        })
        .transpose()?;
    std::fs::create_dir_all(&options.state_dir)?;
    let state_dir = options.state_dir.canonicalize()?;
    let ignore = state_dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "*\n")?;
    }
    if state_dir.join("nodes").exists() && !state_dir.join("ajas").exists() {
        return Err(format!(
            "{} records executions under nodes/, which this version does not read; there is no migration",
            state_dir.display()
        )
        .into());
    }
    Store::new(&state_dir)?;
    let settings = SettingsStore::new(project_dir.clone(), state_dir.clone());
    if let Err(error) = settings.prepare() {
        eprintln!("could not prepare connector config: {error}");
    }
    let effective = settings
        .effective_current()
        .unwrap_or_else(|_| settings.effective(StoredSettings::default()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let host = Arc::new(Host {
        state_dir,
        project_dir,
        settings,
        allowed_origins: options.allowed_origins,
        capabilities: options.capabilities,
        turn_in_flight: AtomicBool::new(false),
        cancelled: cancelled.clone(),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port)).await?;
        let local = listener.local_addr()?;
        println!("eja app  http://{local}");
        println!("  state   {}", host.state_dir.display());
        println!(
            "  project {}",
            host.project_dir
                .as_ref()
                .map_or_else(|| "(none)".to_owned(), |path| path.display().to_string())
        );
        println!(
            "  eja     {}",
            effective
                .roles
                .eja
                .as_ref()
                .map_or("(not configured)", |role| role.model.as_str())
        );
        println!(
            "  aja     {}",
            effective
                .roles
                .aja
                .as_ref()
                .map_or("(not configured)", |role| role.model.as_str())
        );
        let cors_host = host.clone();
        let pulse_host = host.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PULSE.tick);
            loop {
                ticker.tick().await;
                if pulse_host.cancelled.load(Ordering::SeqCst)
                    || pulse_host.turn_in_flight.load(Ordering::SeqCst)
                {
                    continue;
                }
                let Ok(effective) = pulse_host.settings.effective_current() else {
                    continue;
                };
                if !effective.model_ready() {
                    continue;
                }
                let Ok(events) = eja::read_journal(&pulse_host.state_dir) else {
                    continue;
                };
                let Some(last) = events.last() else {
                    continue;
                };
                if last.kind == Kind::Alone || eja::now_utc().timestamp() - last.at < PULSE.quiet {
                    continue;
                }
                if pulse_host.turn_in_flight.swap(true, Ordering::SeqCst) {
                    continue;
                }
                // The quiet window is re-checked under the turn lock: a user
                // turn may have landed between the read above and this claim.
                let Ok(store) = Store::new(&pulse_host.state_dir) else {
                    pulse_host.turn_in_flight.store(false, Ordering::SeqCst);
                    continue;
                };
                let Ok(_lease) = store.lock_turn() else {
                    pulse_host.turn_in_flight.store(false, Ordering::SeqCst);
                    continue;
                };
                let quiet = store.events().ok().and_then(|events| {
                    events.last().map(|last| {
                        last.kind != Kind::Alone
                            && eja::now_utc().timestamp() - last.at >= PULSE.quiet
                    })
                });
                if quiet != Some(true) {
                    pulse_host.turn_in_flight.store(false, Ordering::SeqCst);
                    continue;
                }
                let pulse = pulse_host.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let _reset = TurnFlag(&pulse.turn_in_flight);
                    let _ = execute_turn(
                        &pulse,
                        "",
                        None,
                        Vec::new(),
                        Vec::new(),
                        Some(Kind::Alone),
                        None,
                    );
                })
                .await;
            }
        });
        let app = Router::new()
            .route("/api/state", get(get_state))
            .route("/api/settings", get(get_settings).post(save_settings))
            .route("/api/provider/discover", post(discover_provider))
            .route("/api/connectors/test", post(test_connector))
            .route("/api/turn", post(run_turn))
            .route("/api/raw", get(get_raw))
            .route("/api/health", get(get_health))
            .fallback(api_not_found)
            .layer(middleware::from_fn_with_state(cors_host, cors))
            .with_state(host);
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = tokio::signal::ctrl_c().await;
                cancelled.store(true, Ordering::SeqCst);
            })
            .await?;
        Ok(())
    })
}

async fn get_state(
    State(host): State<Arc<Host>>,
    Query(query): Query<StateQuery>,
) -> ApiResult<Json<Value>> {
    let encounter = query.anchor.map(|anchor| state::Encounter {
        anchor,
        task: query.task_id,
        proposal: query.proposal_id,
    });
    let busy = !matches!(query.busy.as_str(), "" | "0" | "false");
    Ok(Json(state_json(&host, busy, encounter)))
}

fn state_json(host: &Host, busy: bool, encounter: Option<state::Encounter>) -> Value {
    let journal = state::Journal::read(&host.state_dir);
    let effective = host
        .settings
        .effective_current()
        .unwrap_or_else(|_| host.settings.effective(StoredSettings::default()));
    let capabilities = host
        .capabilities
        .with_workspace_tools(effective.workspace_tools);
    let workspace_tools = (capabilities.has(Capability::Files)
        || capabilities.has(Capability::Shell))
        && !capabilities
            .grants(&host.settings, &effective.settings)
            .is_empty();
    let health = state::Health {
        model: effective.model_ready(),
        model_name: effective
            .roles
            .eja
            .as_ref()
            .map_or_else(String::new, |role| role.model.clone()),
        state: host.state_dir.is_dir(),
        mcp: workspace_tools || (capabilities.has(Capability::Mcp) && effective.mcp_ready()),
        journal: if journal.error.is_some() {
            "error"
        } else {
            "ok"
        },
        journal_error: journal.error.clone(),
        torn: journal.torn,
        turn_in_flight: host.turn_in_flight.load(Ordering::SeqCst),
    };
    state::state_json(
        &journal,
        &health,
        busy || host.turn_in_flight.load(Ordering::SeqCst),
        encounter,
    )
}

async fn get_settings(State(host): State<Arc<Host>>) -> ApiResult<Json<Value>> {
    let effective = host.settings.effective_current()?;
    Ok(Json(serde_json::to_value(host.settings.view(&effective))?))
}

async fn save_settings(State(host): State<Arc<Host>>, request: Request) -> ApiResult<Json<Value>> {
    let body = read_json(request).await?;
    host.settings.write(body)?;
    let effective = host.settings.effective_current()?;
    Ok(Json(serde_json::to_value(host.settings.view(&effective))?))
}

async fn discover_provider(
    State(host): State<Arc<Host>>,
    request: Request,
) -> ApiResult<Json<Value>> {
    let body = read_json(request).await?;
    let current = host.settings.read()?;
    let (base_url, api_key, headers) = settings::provider_draft(&body, &current)?;
    let started = Instant::now();
    let result = discover_models(&base_url, &api_key, &headers).await;
    Ok(Json(match result {
        Ok(mut models) => {
            models.sort();
            models.dedup();
            json!({
                "ok": true,
                "models": models.iter().take(1000).collect::<Vec<_>>(),
                "total": models.len(),
                "latencyMs": elapsed_ms(started),
            })
        }
        Err(error) => {
            let secrets = std::iter::once(api_key.clone())
                .chain(headers.values().cloned())
                .filter(|value| value.len() > 7)
                .collect::<Vec<_>>();
            json!({
                "ok": false,
                "error": redact(&error, &secrets),
                "latencyMs": elapsed_ms(started),
            })
        }
    }))
}

fn redact(value: &str, secrets: &[String]) -> String {
    let mut output = value.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            output = output.replace(secret, "••••");
        }
    }
    let mut clean = String::new();
    let mut rest = output.as_str();
    while let Some(at) = rest.to_ascii_lowercase().find("bearer ") {
        clean.push_str(&rest[..at + 7]);
        clean.push_str("••••");
        rest = &rest[at + 7..];
        let end = rest
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';'))
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    clean.push_str(rest);
    clip(&clean, 640)
}

async fn discover_models(
    base_url: &str,
    api_key: &str,
    headers: &BTreeMap<String, String>,
) -> std::result::Result<Vec<String>, String> {
    let base = util::normalize_base_url(base_url).map_err(|error| error.message)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(PROVIDER_TIMEOUT_MS))
        .build()
        .map_err(|error| format!("连不上 Provider：{error}"))?;
    let mut request = client
        .get(format!("{base}/models"))
        .header(header::ACCEPT, "application/json");
    let has_authorization = headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"));
    if !api_key.is_empty() && !has_authorization {
        request = request.bearer_auth(api_key);
    }
    for (name, value) in headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "connection" | "content-length" | "content-type" | "host" | "transfer-encoding"
        ) {
            continue;
        }
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            "连不上 Provider：请求超过 15 秒".to_owned()
        } else if error.is_connect() {
            "连不上 Provider：端点拒绝连接，服务可能没有启动".to_owned()
        } else {
            format!("连不上 Provider：{error}")
        }
    })?;
    let status = response.status();
    let bytes = read_response(response, MAX_BODY)
        .await
        .map_err(|error| format!("Provider 响应读取失败：{error}"))?;
    let value: Option<Value> = serde_json::from_slice(&bytes).ok();
    if !status.is_success() {
        let detail = value
            .as_ref()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message").or(Some(error)))
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
            })
            .map_or_else(
                || String::from_utf8_lossy(&bytes).into_owned(),
                ToOwned::to_owned,
            );
        return Err(describe_provider_error(
            "模型列表",
            status.as_u16(),
            &detail,
        ));
    }
    let source = value
        .as_ref()
        .and_then(|value| value.get("data").and_then(Value::as_array))
        .or_else(|| {
            value
                .as_ref()
                .and_then(|value| value.get("models").and_then(Value::as_array))
        })
        .or_else(|| value.as_ref().and_then(Value::as_array))
        .ok_or_else(|| "响应里没有模型列表".to_owned())?;
    let mut models = Vec::new();
    for item in source {
        let name = item
            .as_str()
            .or_else(|| item.get("id").and_then(Value::as_str))
            .or_else(|| item.get("name").and_then(Value::as_str));
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            models.push(name.to_owned());
        }
    }
    Ok(models)
}

async fn read_response(
    mut response: reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len() + chunk.len() > limit {
            return Err("响应过大".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn describe_provider_error(action: &str, status: u16, detail: &str) -> String {
    let prefix = match status {
        401 | 403 => "API 密钥无效，或没有访问权限",
        404 => return format!("{action}端点不存在，请检查 Base URL"),
        429 => "请求太频繁，或账户额度不足",
        500..=599 => "Provider 暂时不可用",
        _ => return format!("Provider 返回 HTTP {status}"),
    };
    let detail = clip(detail.trim(), 640);
    if detail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}：{detail}")
    }
}

async fn test_connector(State(host): State<Arc<Host>>, request: Request) -> ApiResult<Json<Value>> {
    let body = read_json(request).await?;
    let started = Instant::now();
    let connector = match host.settings.connector_for_test(&body) {
        Ok(connector) => connector,
        Err(error) => {
            return Ok(Json(json!({
                "ok": false,
                "error": error.message,
                "latencyMs": 0,
            })))
        }
    };
    let project = host.project_dir.clone();
    let state_dir = host.state_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        probe_connector(&connector, project.as_deref(), &state_dir)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(match result {
        Ok((tools, stderr)) => json!({
            "ok": true,
            "tools": tools,
            "count": tools.len(),
            "latencyMs": elapsed_ms(started),
            "stderr": stderr,
        }),
        Err(error) => json!({
            "ok": false,
            "error": error,
            "latencyMs": elapsed_ms(started),
        }),
    }))
}

fn probe_connector(
    connector: &StoredConnector,
    project: Option<&Path>,
    state_dir: &Path,
) -> std::result::Result<(Vec<Value>, String), String> {
    std::fs::create_dir_all(state_dir).map_err(|error| error.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let root = std::env::temp_dir().join(format!("eja-mcp-test-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let config = root.join("mcp.json");
    let mut probe = connector.clone();
    probe.enabled = true;
    let mut servers = serde_json::Map::new();
    servers.insert(probe.name.clone(), connector_config(&probe));
    let document = json!({
        "mcpServers": servers
    });
    util::write_private_json(&config, &document).map_err(|error| error.message)?;
    let timeout_ms = CONNECTOR_TEST_LIMIT_MS.min(connector.timeout_ms.saturating_add(5_000));
    let control = Control::new(
        Duration::from_millis(timeout_ms),
        Arc::new(AtomicBool::new(false)),
    );
    let result = (|| {
        let tools = HostTools::with_workspace(Some(&config), project, state_dir, &control)
            .map_err(|error| sanitize(&error.to_string(), connector))?;
        let definitions = tools.definitions(&Assignment {
            task_id: 0,
            objective: String::new(),
            workspace: String::new(),
            acceptance: String::new(),
            input: String::new(),
            resume: None,
            issued_at: eja::now_utc().timestamp(),
        });
        Ok::<_, String>(connector_tools(&definitions, &connector.name))
    })();
    let _ = std::fs::remove_dir_all(&root);
    result.map(|tools| (tools, String::new()))
}

fn connector_tools(definitions: &[Value], server: &str) -> Vec<Value> {
    let prefix = format!("mcp__{server}__");
    let marker = format!("MCP {server}/");
    let mut tools = Vec::new();
    for definition in definitions {
        let function = definition.get("function");
        let Some(public_name) = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(short) = public_name.strip_prefix(&prefix) else {
            continue;
        };
        let description = function
            .and_then(|function| function.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let (name, summary) = description.strip_prefix(&marker).map_or_else(
            || (short.to_owned(), description.to_owned()),
            |rest| {
                rest.split_once(": ").map_or_else(
                    || (rest.to_owned(), String::new()),
                    |(name, summary)| (name.to_owned(), summary.to_owned()),
                )
            },
        );
        tools.push(json!({
            "name": name,
            "publicName": public_name,
            "description": summary,
        }));
    }
    tools.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    tools
}

fn sanitize(value: &str, connector: &StoredConnector) -> String {
    let mut output = clip(value.trim(), 2400);
    for secret in connector
        .env
        .values()
        .chain(connector.headers.values())
        .filter(|value| value.len() > 7)
    {
        output = output.replace(secret, "••••");
    }
    output
}

async fn run_turn(State(host): State<Arc<Host>>, request: Request) -> ApiResult<Json<Value>> {
    let body = read_json(request).await?;
    let turn: TurnRequest = serde_json::from_value(body)?;
    let text = turn.text.trim();
    let approvals: Vec<u64> = turn.approvals.into_iter().filter(|id| *id > 0).collect();
    let denials: Vec<u64> = turn.denials.into_iter().filter(|id| *id > 0).collect();
    let gesture = match turn.gesture.as_deref() {
        None | Some("") => None,
        Some("return") => Some(Kind::Returned),
        Some("leave") => Some(Kind::Departed),
        Some("alone") => Some(Kind::Alone),
        Some(_) => return Err(ApiError::bad("unknown gesture")),
    };
    if turn.selection.is_some()
        && (!text.is_empty() || !approvals.is_empty() || !denials.is_empty() || gesture.is_some())
    {
        return Err(ApiError::bad("a choice answer is its own turn input"));
    }
    if text.is_empty()
        && turn.selection.is_none()
        && approvals.is_empty()
        && denials.is_empty()
        && gesture.is_none()
    {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "a turn needs text, a choice, an approval, a denial or a situation".to_owned(),
        });
    }
    if gesture.is_some() && !approvals.is_empty() {
        return Err(ApiError::bad("a situation carries no approvals"));
    }
    if gesture.is_some() && !denials.is_empty() {
        return Err(ApiError::bad("a situation carries no denials"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("text is longer than {MAX_TEXT} characters"),
        });
    }
    if host.turn_in_flight.swap(true, Ordering::SeqCst) {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "a turn is already running".to_owned(),
        });
    }
    let _reset = TurnFlag(&host.turn_in_flight);
    let started = Instant::now();
    let text = text.to_owned();
    let host_for_turn = host.clone();
    let reply = tokio::task::spawn_blocking(move || {
        execute_turn(
            &host_for_turn,
            &text,
            turn.focus.filter(|id| *id > 0),
            approvals,
            denials,
            gesture,
            turn.selection,
        )
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))??;
    Ok(Json(json!({
        "ok": true,
        "elapsedMs": elapsed_ms(started),
        "stdout": clip(&reply, MAX_OUTPUT),
        "stderr": "",
    })))
}

fn execute_turn(
    host: &Host,
    text: &str,
    focus: Option<u64>,
    approvals: Vec<u64>,
    denials: Vec<u64>,
    gesture: Option<Kind>,
    selection: Option<ChoiceSelection>,
) -> ApiResult<String> {
    let effective = host.settings.effective_current()?;
    if !effective.model_ready() {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message:
                "model is not configured. Open settings and choose a provider and model for eja."
                    .to_owned(),
        });
    }
    let eja_role = effective
        .roles
        .eja
        .as_ref()
        .ok_or_else(|| ApiError::internal("eja role is not configured"))?;
    let aja_role = effective
        .roles
        .aja
        .as_ref()
        .ok_or_else(|| ApiError::internal("aja role is not configured"))?;
    let brain = OpenAiBrain::new(
        eja_role.base_url.as_str(),
        eja_role.api_key.as_str(),
        eja_role.model.as_str(),
    )
    .with_headers(header_pairs(eja_role))
    .with_managed_plugins(host.capabilities.has(Capability::Plugins));
    let model = OpenAiModel::new(
        aja_role.base_url.as_str(),
        aja_role.api_key.as_str(),
        aja_role.model.as_str(),
    )
    .with_headers(header_pairs(aja_role));
    let startup = Control::new(Duration::from_secs(300), Arc::new(AtomicBool::new(false)));
    let config = effective.mcp_path.as_deref().filter(|path| path.is_file());
    let grant = host
        .capabilities
        .grants(&host.settings, &effective.settings);
    let assembled = assemble_capabilities(
        host.project_dir.as_deref(),
        &host.state_dir,
        config,
        &grant,
        host.capabilities
            .with_workspace_tools(effective.workspace_tools),
        effective.limits.slots,
        &startup,
    )?;
    let ajas = host.state_dir.join("ajas");
    let mut aja_limits = eja::aja::Limits::default();
    if effective.limits.aja_steps > 0 {
        aja_limits.steps = Some(effective.limits.aja_steps);
    }
    if effective.limits.aja_timeout_ms > 0 {
        aja_limits.timeout_ms = effective.limits.aja_timeout_ms;
    }
    let worker = assembled.worker(&ajas, model, aja_limits)?;
    // Reading a historical checkpoint is independent of creating new ones.
    let records = RecordFiles::with_checkpoints(&ajas, host.state_dir.join("checkpoints"));
    let ports = Ports {
        aja: Some(&records),
        capabilities: Some(&*assembled.tools),
        worlds: assembled.worlds.as_deref(),
    };
    let store = Store::new(&host.state_dir)?;
    let mut options = TurnOptions {
        approvals,
        denials,
        selection,
        focus,
        gesture,
        workspaces: grant,
        cancelled: host.cancelled.clone(),
        ..TurnOptions::default()
    };
    options.limits.slots = effective.limits.slots.max(1);
    if effective.limits.turn_timeout_ms > 0 {
        options.limits.timeout_ms = effective.limits.turn_timeout_ms;
    }
    let outcome = turn_at_with_options(
        &store,
        text,
        &brain,
        &worker,
        &ports,
        eja::now_utc(),
        &options,
    )
    .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(outcome.reply)
}

/// One deployment's assembled optional capabilities.
pub struct Assembled {
    pub tools: Arc<HostTools>,
    pub worlds: Option<Arc<dyn Worlds>>,
    workspace: Option<PathBuf>,
    git: bool,
}

impl Assembled {
    pub fn worker(
        &self,
        ajas: &Path,
        model: OpenAiModel,
        limits: eja::aja::Limits,
    ) -> eja::Result<NativeWorker> {
        let mut worker = NativeWorker::for_workspace(
            std::env::current_exe()?,
            ajas,
            self.workspace.as_deref(),
            Arc::new(model),
            self.tools.clone() as Arc<dyn ToolGateway>,
        )?
        .with_limits(limits)?;
        if !self.git {
            worker = worker.without_checkpoints();
        }
        Ok(if let Some(worlds) = &self.worlds {
            worker.with_worlds(worlds.clone())
        } else {
            worker
        })
    }
}

/// Assemble local tools, connectors, worlds and Git evidence from the host's
/// grants. With no workspace and no grant this is connector-only: no file tools,
/// no project cwd, and no checkpoint repository.
pub fn assemble_capabilities(
    project: Option<&Path>,
    state_dir: &Path,
    config: Option<&Path>,
    grant: &[eja::WorkspaceGrant],
    capabilities: EnabledCapabilities,
    slots: usize,
    startup: &Control,
) -> eja::Result<Assembled> {
    let workspace = project
        .map(Path::to_path_buf)
        .or_else(|| grant.first().map(|grant| PathBuf::from(&grant.root)));
    let mut tools = HostTools::with_workspace(
        config.filter(|_| capabilities.has(Capability::Mcp)),
        workspace.as_deref(),
        state_dir,
        startup,
    )?
    .with_local_tools(
        capabilities.has(Capability::Files),
        capabilities.has(Capability::Shell),
    )
    .with_roots(
        grant
            .iter()
            .map(|workspace| PathBuf::from(&workspace.root))
            .collect(),
    );
    if capabilities.has(Capability::Plugins) {
        plugins::register(&mut tools, state_dir, startup)?;
    }
    let worlds = build_worlds(
        state_dir,
        grant,
        slots,
        capabilities.has(Capability::Git),
        startup,
    );
    Ok(Assembled {
        tools: Arc::new(tools),
        worlds,
        workspace,
        git: capabilities.has(Capability::Git),
    })
}

fn header_pairs(role: &RoleConfig) -> Vec<(String, String)> {
    role.headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// One backend per granted root. With parallel slots a git root gets isolated
/// worktrees and any other root shares itself; with one slot every root is
/// shared. The default root is the first grant, so a task that names nothing
/// works where the host was started.
fn build_worlds(
    state_dir: &Path,
    grant: &[eja::WorkspaceGrant],
    slots: usize,
    git: bool,
    startup: &Control,
) -> Option<Arc<dyn Worlds>> {
    let first_root = normalize_root(&grant.first()?.root);
    let mut roots: BTreeMap<PathBuf, Arc<dyn Worlds>> = BTreeMap::new();
    for (index, grant) in grant.iter().enumerate() {
        let path = normalize_root(&grant.root);
        if roots.contains_key(&path) {
            continue;
        }
        let backend: Arc<dyn Worlds> = if slots > 1 && git {
            match Worktrees::new(
                &path,
                state_dir.join("worlds").join(index.to_string()),
                state_dir,
                startup,
            ) {
                Ok(worlds) => Arc::new(worlds),
                Err(_) => Arc::new(Shared::new(path.clone(), state_dir.to_path_buf())),
            }
        } else {
            Arc::new(Shared::new(path.clone(), state_dir.to_path_buf()))
        };
        roots.insert(path, backend);
    }
    let default = roots.get(&first_root)?.clone();
    Some(Arc::new(eja::adapters::WorkspaceWorlds::new(
        default, roots,
    )))
}

/// Grant keys are real paths: a granted root reached through a symlink is the
/// same world as the resolved one, never a second backend beside it.
fn normalize_root(root: &str) -> PathBuf {
    PathBuf::from(root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(root))
}

async fn get_raw(
    State(host): State<Arc<Host>>,
    Query(query): Query<RawQuery>,
) -> ApiResult<Json<Value>> {
    let address = query.address;
    let journal = state::Journal::read(&host.state_dir);
    if let Some(raw) = address.strip_prefix("log:event:") {
        let id = raw.parse().map_err(|_| ApiError::bad("invalid event id"))?;
        let event =
            state::journal_event(&journal, id).ok_or_else(|| ApiError::missing("no such event"))?;
        return Ok(Json(json!({
            "address": address,
            "kind": "json",
            "content": serde_json::to_string_pretty(&event)?,
        })));
    }
    if let Some(raw) = address.strip_prefix("log:task:") {
        let id = raw.parse().map_err(|_| ApiError::bad("invalid task id"))?;
        let task =
            state::journal_task(&journal, id).ok_or_else(|| ApiError::missing("no such task"))?;
        return Ok(Json(json!({
            "address": address,
            "kind": "json",
            "content": serde_json::to_string_pretty(&task)?,
        })));
    }
    if let Some(rest) = address.strip_prefix("record:") {
        let (id, relative) = rest
            .split_once('/')
            .ok_or_else(|| ApiError::bad("record address needs an aja and a path"))?;
        let id: u64 = id.parse().map_err(|_| ApiError::bad("invalid aja id"))?;
        if id == 0 {
            return Err(ApiError::bad("invalid aja id"));
        }
        let root = host.state_dir.join("ajas").join(id.to_string());
        let (content, bytes, truncated) = safe_file(&root, relative, MAX_RAW)?;
        return Ok(Json(json!({
            "address": address,
            "kind": "text",
            "content": content,
            "bytes": bytes,
            "truncated": truncated,
        })));
    }
    if let Some(relative) = address.strip_prefix("world:") {
        let grant = host.capabilities.grants(
            &host.settings,
            &host
                .settings
                .effective_current()
                .unwrap_or_else(|_| host.settings.effective(StoredSettings::default()))
                .settings,
        );
        let mut found = None;
        for workspace in &grant {
            if let Ok(view) = safe_file(Path::new(&workspace.root), relative, MAX_RAW) {
                found = Some(view);
                break;
            }
        }
        let (content, bytes, truncated) =
            found.ok_or_else(|| ApiError::missing("not found in any granted workspace"))?;
        return Ok(Json(json!({
            "address": address,
            "kind": "text",
            "content": content,
            "bytes": bytes,
            "truncated": truncated,
        })));
    }
    Err(ApiError::bad(
        "unknown address; use log:, record:, or world:",
    ))
}

async fn get_health(State(host): State<Arc<Host>>) -> Json<Value> {
    Json(state_json(&host, false, None)["health"].clone())
}

async fn api_not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn cors(State(host): State<Arc<Host>>, request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .filter(|origin| {
            local_origin(origin) || host.allowed_origins.iter().any(|item| item == origin)
        })
        .map(ToOwned::to_owned);
    if host.capabilities.has(Capability::Plugins) && request.method() == Method::POST {
        let allowed_origin = origin.is_some() || !request.headers().contains_key(header::ORIGIN);
        let json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
            });
        if !allowed_origin || !json {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if request.method() == Method::OPTIONS {
        if origin.is_none() && request.headers().contains_key(header::ORIGIN) {
            return StatusCode::FORBIDDEN.into_response();
        }
        let mut response = StatusCode::NO_CONTENT.into_response();
        apply_cors(response.headers_mut(), origin.as_deref());
        return response;
    }
    let mut response = next.run(request).await;
    apply_cors(response.headers_mut(), origin.as_deref());
    response
}

fn local_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let host = if host.starts_with('[') {
        host.split(']').next().map_or(host, |host| &host[1..])
    } else {
        host.rsplit_once(':').map_or(host, |(host, _)| host)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn apply_cors(headers: &mut axum::http::HeaderMap, origin: Option<&str>) {
    headers.insert(
        header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    let Some(origin) = origin else {
        return;
    };
    if let Ok(origin) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, authorization"),
    );
}

async fn read_json(request: Request) -> ApiResult<Value> {
    let body = request.into_body();
    let bytes = to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| ApiError::bad("request body too large"))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("request body must be JSON"))
}
