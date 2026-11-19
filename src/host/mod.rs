mod capability;
mod connectors;
mod plugin_sandbox;
mod plugins;
mod settings;
mod state;
mod util;

mod compose;

pub use compose::compose;
pub use settings::SettingsStore;

use self::{
    capability::{FILES, MANAGED_PLUGINS, MCP, SHELL},
    connectors::{connector_config, StoredConnector},
    util::{clip, safe_file, ApiError, ApiResult},
};
use axum::{
    body::to_bytes,
    extract::{Path as ApiPath, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use eja::{
    adapters::{Control, McpTools, RecordFiles, RecordReader, ToolProvider},
    validate_turn_input, Assignment, ChoiceSelection, JournalLease, Kind, Selector, Store,
    TurnOptions,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
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
}

pub struct Host {
    state_dir: PathBuf,
    project_dir: Option<PathBuf>,
    settings: SettingsStore,
    allowed_origins: Vec<String>,
    local: bool,
    turn_in_flight: AtomicBool,
    cancelled: Arc<AtomicBool>,
    state_cache: Mutex<state::Cache>,
    /// The most recent optional-capability failures. They are diagnostics, not
    /// turn failures: a broken connector never disables local tools or judgment.
    diagnostics: Mutex<Vec<String>>,
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
    #[serde(default)]
    after: u64,
}

pub fn serve(options: Options) -> eja::Result<()> {
    if options.port == 0 {
        return Err("--port must be between 1 and 65535".into());
    }
    let local = options.host == "localhost"
        || options
            .host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    let project_dir = options
        .project_dir
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
    Store::new(&state_dir)?;
    let ignore = state_dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "*\n")?;
    }
    let settings = SettingsStore::new(project_dir.clone(), state_dir.clone());
    let effective = settings.effective_current()?;
    if effective.managed_code_enabled() && !local {
        return Err("plugin administration requires a local runtime address".into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let cache_dir = state_dir.clone();
    let host = Arc::new(Host {
        state_dir,
        project_dir,
        settings,
        allowed_origins: options.allowed_origins,
        local,
        turn_in_flight: AtomicBool::new(false),
        cancelled: cancelled.clone(),
        state_cache: Mutex::new(state::Cache::new(cache_dir)),
        diagnostics: Mutex::new(Vec::new()),
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
                let pulse = pulse_host.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _reset = TurnFlag(&pulse.turn_in_flight);
                    pulse_once(&pulse)
                })
                .await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => eprintln!("pulse: {}", error.message),
                    Err(error) => eprintln!("pulse task failed: {error}"),
                }
            }
        });
        let app = Router::new()
            .route("/api/state", get(get_state))
            .route("/api/settings", get(get_settings).post(save_settings))
            .route("/api/capabilities", get(get_capabilities))
            .route("/api/provider/discover", post(discover_provider))
            .route("/api/capabilities/{id}/probe", post(probe_capability))
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
    Ok(Json(state_json(&host, busy, encounter, query.after)?))
}

#[allow(clippy::significant_drop_tightening)] // One lock covers one coherent cache snapshot.
fn state_json(
    host: &Host,
    busy: bool,
    encounter: Option<state::Encounter>,
    after: u64,
) -> ApiResult<Value> {
    let effective = host.settings.effective_current()?;
    let capabilities = &effective.settings.capabilities;
    let workspace_tools = (capabilities.enabled(FILES) || capabilities.enabled(SHELL))
        && !effective.grants(&host.settings).is_empty();
    let plugin_tools = effective.managed_code_enabled();
    let diagnostics = host
        .diagnostics
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut health = state::Health {
        model: effective.model_ready(),
        model_name: effective
            .roles
            .eja
            .as_ref()
            .map_or_else(String::new, |role| role.model.clone()),
        state: host.state_dir.is_dir(),
        tools: workspace_tools
            || plugin_tools
            || (capabilities.enabled(MCP) && effective.mcp_ready()),
        journal: "ok",
        journal_error: None,
        torn: false,
        turn_in_flight: host.turn_in_flight.load(Ordering::SeqCst),
        capability_errors: diagnostics,
    };
    let view = {
        let mut cache = host
            .state_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let journal = cache.journal();
        health.journal = if journal.error.is_some() {
            "error"
        } else {
            "ok"
        };
        health.journal_error.clone_from(&journal.error);
        health.torn = journal.torn;
        state::state_json(
            &mut cache,
            after,
            &health,
            busy || host.turn_in_flight.load(Ordering::SeqCst),
            encounter,
        )
    };
    Ok(view)
}

async fn get_settings(State(host): State<Arc<Host>>) -> ApiResult<Json<Value>> {
    let effective = host.settings.effective_current()?;
    Ok(Json(serde_json::to_value(host.settings.view(&effective))?))
}

async fn get_capabilities(State(host): State<Arc<Host>>) -> ApiResult<Json<Value>> {
    let effective = host.settings.effective_current()?;
    let view = host.settings.view(&effective);
    Ok(Json(serde_json::to_value(view.capabilities)?))
}

async fn save_settings(State(host): State<Arc<Host>>, request: Request) -> ApiResult<Json<Value>> {
    let body = read_json(request).await?;
    if !host.local && plugin_admin_requested(&body) {
        return Err(ApiError::bad(
            "plugin administration requires a local runtime address",
        ));
    }
    host.settings.write(body)?;
    let effective = host.settings.effective_current()?;
    Ok(Json(serde_json::to_value(host.settings.view(&effective))?))
}

fn plugin_admin_requested(body: &Value) -> bool {
    body.get("capabilities")
        .and_then(Value::as_object)
        .is_some_and(|capabilities| {
            capabilities.iter().any(|(id, capability)| {
                (id == MANAGED_PLUGINS || id.starts_with("plugin:"))
                    && capability.get("enabled").and_then(Value::as_bool) == Some(true)
            })
        })
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

async fn probe_capability(
    ApiPath(id): ApiPath<String>,
    State(host): State<Arc<Host>>,
    request: Request,
) -> ApiResult<Json<Value>> {
    if id != MCP {
        return Err(ApiError::missing("this capability has no host-side probe"));
    }
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
        let tools = McpTools::load_strict(&config, project, &control)
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
    let approvals = turn.approvals;
    let denials = turn.denials;
    let gesture = match turn.gesture.as_deref() {
        None | Some("") => None,
        Some("return") => Some(Kind::Returned),
        Some("leave") => Some(Kind::Departed),
        Some("alone") => Some(Kind::Alone),
        Some(_) => return Err(ApiError::bad("unknown gesture")),
    };
    let input_options = TurnOptions {
        approvals: approvals.clone(),
        denials: denials.clone(),
        selection: turn.selection,
        focus: turn.focus,
        gesture,
        ..TurnOptions::default()
    };
    validate_turn_input(text, &input_options).map_err(|error| ApiError::bad(error.to_string()))?;
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
    let input = TurnInput {
        text: text.to_owned(),
        focus: turn.focus,
        approvals,
        denials,
        gesture,
        selection: turn.selection,
    };
    let host_for_turn = host.clone();
    let reply = tokio::task::spawn_blocking(move || {
        execute_turn(
            &host_for_turn,
            &Store::new(&host_for_turn.state_dir)
                .map_err(|error| ApiError::internal(error.to_string()))?,
            input,
            None,
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

struct TurnInput {
    text: String,
    focus: Option<u64>,
    approvals: Vec<u64>,
    denials: Vec<u64>,
    gesture: Option<Kind>,
    selection: Option<ChoiceSelection>,
}

fn execute_turn(
    host: &Host,
    store: &Store,
    input: TurnInput,
    lease: Option<&JournalLease>,
) -> ApiResult<String> {
    let startup = Control::new(Duration::from_secs(300), Arc::new(AtomicBool::new(false)));
    let composed = compose(&host.settings, &startup)?;
    {
        let mut current = host
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.clone_from(&composed.diagnostics);
        for diagnostic in &*current {
            eprintln!("capability warning: {diagnostic}");
        }
    }
    let effective = &composed.effective;
    if !effective.model_ready() {
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message:
                "model is not configured. Open settings and choose a provider and model for eja."
                    .to_owned(),
        });
    }
    let options = TurnOptions {
        approvals: input.approvals,
        denials: input.denials,
        selection: input.selection,
        focus: input.focus,
        gesture: input.gesture,
        cancelled: host.cancelled.clone(),
        ..TurnOptions::default()
    };
    let outcome = composed
        .run_turn(&host.state_dir, store, &input.text, &options, lease)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    if outcome.end == eja::TurnEnd::Failed {
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: outcome
                .reason
                .unwrap_or_else(|| "model transport failed".to_owned()),
        });
    }
    Ok(outcome.reply)
}

/// Re-check the quiet window and enter EJA while holding one journal lease. The
/// timer's earlier reads only avoid claiming a busy process; this is the check
/// that decides whether the pulse may write.
fn pulse_once(host: &Host) -> ApiResult<bool> {
    let store = Store::new(&host.state_dir)?;
    let Some(lease) = quiet_lease(&store, PULSE.quiet)? else {
        return Ok(false);
    };
    execute_turn(
        host,
        &store,
        TurnInput {
            text: String::new(),
            focus: None,
            approvals: Vec::new(),
            denials: Vec::new(),
            gesture: Some(Kind::Alone),
            selection: None,
        },
        Some(&lease),
    )?;
    Ok(true)
}

/// The one locking rule for a pulse: acquire first, then read the journal and
/// decide whether quiet is still true. A busy journal is not a pulse error.
fn quiet_lease(store: &Store, quiet: i64) -> eja::Result<Option<JournalLease>> {
    let lease = match store.lock_turn() {
        Ok(lease) => lease,
        Err(error) if error.to_string() == "another turn already owns this journal" => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let events = store.events()?;
    let Some(last) = events.last() else {
        return Ok(None);
    };
    if last.kind == Kind::Alone || eja::now_utc().timestamp() - last.at < quiet {
        return Ok(None);
    }
    Ok(Some(lease))
}

async fn get_raw(
    State(host): State<Arc<Host>>,
    Query(query): Query<RawQuery>,
) -> ApiResult<Json<Value>> {
    let address = query.address;
    if let Some(raw) = address.strip_prefix("log:event:") {
        let id = raw.parse().map_err(|_| ApiError::bad("invalid event id"))?;
        let event = host
            .state_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .raw_event(id)
            .ok_or_else(|| ApiError::missing("no such event"))?;
        return Ok(Json(json!({
            "address": address,
            "kind": "json",
            "content": serde_json::to_string_pretty(&event)?,
        })));
    }
    if let Some(raw) = address.strip_prefix("log:task:") {
        let id = raw.parse().map_err(|_| ApiError::bad("invalid task id"))?;
        let task = host
            .state_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .raw_task(id)
            .ok_or_else(|| ApiError::missing("no such task"))?;
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
        let reader = RecordFiles::with_checkpoints_and_limit(
            host.state_dir.join("ajas"),
            host.state_dir.join("checkpoints"),
            MAX_RAW,
        );
        let control = Control::new(Duration::from_secs(10), Arc::new(AtomicBool::new(false)));
        let view = reader
            .slice(&format!("{id}/{relative}"), &Selector::Whole, &control)
            .map_err(|error| ApiError::missing(error.to_string()))?;
        let content = view
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let bytes = view
            .get("bytes")
            .and_then(Value::as_u64)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(content.len());
        let truncated = view
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Ok(Json(json!({
            "address": address,
            "kind": "text",
            "content": content,
            "bytes": bytes,
            "truncated": truncated,
        })));
    }
    if let Some(relative) = address.strip_prefix("environment:") {
        let effective = host.settings.effective_current()?;
        let grant = effective.grants(&host.settings);
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
        "unknown address; use log:, record:, or environment:",
    ))
}

async fn get_health(State(host): State<Arc<Host>>) -> ApiResult<Json<Value>> {
    Ok(Json(
        state_json(&host, false, None, u64::MAX)?["health"].clone(),
    ))
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
    let plugins = host
        .settings
        .effective_current()
        .is_ok_and(|effective| effective.managed_code_enabled());
    if plugins && request.method() == Method::POST {
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

#[cfg(test)]
mod tests {
    use super::quiet_lease;
    use eja::adapters::Ports;
    use eja::{Brain, Decision, EjaContext, Kind, Store, TurnEnd, TurnOptions, TurnRun, Worker};
    use std::{
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Barrier,
        },
    };

    struct SilentBrain;

    impl Brain for SilentBrain {
        fn decide(&self, _: &EjaContext, _: &eja::Control) -> eja::Result<Decision> {
            Ok(Decision::default())
        }
    }

    struct SilentWorker;

    impl Worker for SilentWorker {
        fn execute(&self, _: &eja::Assignment, _: &eja::Control) -> String {
            "no run".to_owned()
        }
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "eja-pulse-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run_alone(store: &Store, lease: &eja::JournalLease) -> eja::TurnOutcome<'static> {
        let brain = SilentBrain;
        let worker = SilentWorker;
        let ports = Ports::default();
        TurnRun {
            store,
            input: "",
            brain: &brain,
            worker: &worker,
            host: &ports,
            now: eja::now_utc(),
            options: &TurnOptions {
                gesture: Some(Kind::Alone),
                ..TurnOptions::default()
            },
        }
        .execute_under_lease(lease)
        .expect("pulse turn")
    }

    #[test]
    fn a_quiet_journal_runs_one_alone_turn_under_the_same_lease() {
        let fixture = Fixture::new("quiet");
        let store = Store::new(fixture.path()).expect("store");
        store
            .append_simple(Kind::User, "old", 1, eja::now_utc().timestamp() - 3_600)
            .expect("old fact");
        let lease = quiet_lease(&store, 60)
            .expect("lease decision")
            .expect("the journal is quiet");
        let outcome = run_alone(&store, &lease);
        assert_eq!(outcome.end, TurnEnd::Complete);
        let events = store.events().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == Kind::Alone)
                .count(),
            1
        );
        assert_eq!(events.last().map(|event| event.kind), Some(Kind::Eja));
    }

    #[test]
    fn a_new_input_before_the_lease_makes_the_pulse_stand_down() {
        let fixture = Fixture::new("not-quiet");
        let store = Store::new(fixture.path()).expect("store");
        store
            .append_simple(Kind::User, "just now", 1, eja::now_utc().timestamp())
            .expect("new fact");
        assert!(quiet_lease(&store, 60).expect("lease decision").is_none());
        assert_eq!(
            store
                .events()
                .expect("events")
                .iter()
                .filter(|event| event.kind == Kind::Alone)
                .count(),
            0
        );
    }

    #[test]
    fn concurrent_pulses_enter_at_most_one_turn() {
        let fixture = Fixture::new("concurrent");
        let store = Store::new(fixture.path()).expect("store");
        store
            .append_simple(Kind::User, "old", 1, eja::now_utc().timestamp() - 3_600)
            .expect("old fact");
        let together = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let together = together.clone();
            threads.push(std::thread::spawn(move || {
                together.wait();
                let Some(lease) = quiet_lease(&store, 60).expect("lease decision") else {
                    return false;
                };
                let outcome = run_alone(&store, &lease);
                assert_eq!(outcome.end, TurnEnd::Complete);
                true
            }));
        }
        let entered = threads
            .into_iter()
            .map(|thread| thread.join().expect("pulse thread"))
            .filter(|entered| *entered)
            .count();
        assert_eq!(entered, 1, "only one pulse owns the turn");
        assert_eq!(
            store
                .events()
                .expect("events")
                .iter()
                .filter(|event| event.kind == Kind::Alone)
                .count(),
            1
        );
    }
}
