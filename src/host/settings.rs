use super::util::{
    is_local_provider, normalize_base_url, provider_name, unique_strings, write_private_bytes,
    write_private_json, ApiError, ApiResult,
};
use eja::WorkspaceGrant;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Role {
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Roles {
    #[serde(default)]
    pub eja: Role,
    #[serde(default)]
    pub aja: Role,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub slots: usize,
    pub turn_timeout_ms: u64,
    pub aja_steps: usize,
    pub aja_timeout_ms: u64,
}

impl Limits {
    fn sane(self) -> Self {
        Self {
            slots: self.slots.clamp(1, 8),
            turn_timeout_ms: self.turn_timeout_ms.min(7_200_000),
            aja_steps: self.aja_steps.min(200),
            aja_timeout_ms: self.aja_timeout_ms.min(14_400_000),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredConnector {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout", rename = "timeout_ms")]
    pub timeout_ms: u64,
}

const fn default_true() -> bool {
    true
}

const fn default_timeout() -> u64 {
    30_000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectorSettings {
    #[serde(default)]
    pub servers: Vec<StoredConnector>,
    #[serde(default)]
    pub managed: bool,
    /// The built-in workspace file tools: always granted, switchable, never
    /// deletable. They are not a connector process, so they live beside the
    /// server list rather than inside it.
    #[serde(default = "default_true")]
    pub workspace_tools: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSettings {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub roles: Roles,
    #[serde(default)]
    pub connectors: ConnectorSettings,
    #[serde(default)]
    pub limits: Limits,
    /// Extra directories the user has made available to EJA. A task fixes one
    /// of these roots when it opens.
    #[serde(default)]
    pub workspaces: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderInput {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    clear_api_key: bool,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    headers: Vec<KeyValueInput>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct KeyValueInput {
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    redacted: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorInput {
    #[serde(default)]
    name: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    env: Vec<KeyValueInput>,
    #[serde(default)]
    headers: Vec<KeyValueInput>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct SaveInput {
    #[serde(default)]
    providers: Vec<ProviderInput>,
    #[serde(default)]
    roles: Roles,
    #[serde(default)]
    connectors: ConnectorSaveInput,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorSaveInput {
    #[serde(default)]
    servers: Vec<ConnectorInput>,
    #[serde(default)]
    workspace_tools: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValueView {
    pub key: String,
    pub value: String,
    pub secret: bool,
    pub has_value: bool,
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub has_api_key: bool,
    pub local: bool,
    pub headers: Vec<KeyValueView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorView {
    pub name: String,
    pub mode: &'static str,
    pub builtin: bool,
    pub command: String,
    pub url: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<KeyValueView>,
    pub headers: Vec<KeyValueView>,
    pub enabled: bool,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorsView {
    pub servers: Vec<ConnectorView>,
    pub managed: bool,
    pub config_path: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub providers: Vec<ProviderView>,
    pub roles: Roles,
    pub connectors: ConnectorsView,
    pub limits: Limits,
    pub workspaces: BTreeMap<String, String>,
    pub runtime_path: String,
    pub state_dir: String,
    pub project_dir: String,
}

#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub headers: BTreeMap<String, String>,
}

impl RoleConfig {
    pub const fn configured(&self) -> bool {
        !self.base_url.is_empty() && !self.model.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveRoles {
    pub eja: Option<RoleConfig>,
    pub aja: Option<RoleConfig>,
}

#[derive(Debug, Clone)]
pub struct ActiveConnectors {
    pub servers: Vec<StoredConnector>,
    pub managed: bool,
    pub error: Option<String>,
    pub workspace_tools: bool,
}

#[derive(Debug, Clone)]
pub struct Effective {
    pub settings: StoredSettings,
    pub connectors: ActiveConnectors,
    pub roles: EffectiveRoles,
    pub mcp_path: Option<PathBuf>,
    pub limits: Limits,
    pub workspace_tools: bool,
}

impl Effective {
    pub fn model_ready(&self) -> bool {
        self.roles.eja.as_ref().is_some_and(RoleConfig::configured)
            && self.roles.aja.as_ref().is_some_and(RoleConfig::configured)
    }

    pub fn mcp_ready(&self) -> bool {
        self.mcp_path.as_ref().is_some_and(|path| path.is_file())
            && self.connectors.servers.iter().any(|server| server.enabled)
    }
}

#[derive(Clone)]
pub struct SettingsStore {
    path: PathBuf,
    project: Option<PathBuf>,
    dir: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl SettingsStore {
    pub fn new(project: Option<PathBuf>, dir: PathBuf) -> Self {
        Self {
            path: dir.join("app-settings.json"),
            project,
            dir,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn read(&self) -> ApiResult<StoredSettings> {
        if !self.path.is_file() {
            if let Some(legacy) = self
                .project
                .as_ref()
                .map(|project| project.join("app/settings.json"))
                .filter(|path| path.is_file())
            {
                let bytes = fs::read(legacy)?;
                write_private_bytes(&self.path, &bytes)?;
            }
        }
        if !self.path.is_file() {
            return Ok(StoredSettings {
                limits: Limits {
                    slots: 1,
                    ..Limits::default()
                },
                connectors: ConnectorSettings {
                    workspace_tools: true,
                    ..ConnectorSettings::default()
                },
                ..StoredSettings::default()
            });
        }
        let raw: Value = serde_json::from_slice(&fs::read(&self.path)?)?;
        let mut settings: StoredSettings = serde_json::from_value(raw.clone()).unwrap_or_default();
        if settings.providers.is_empty() {
            if let Some(provider) = legacy_provider(&raw) {
                let primary = provider.models.first().cloned().unwrap_or_default();
                let aja = provider.models.get(1).cloned().unwrap_or_default();
                if settings.roles.eja.provider_id.is_empty() && settings.roles.eja.model.is_empty()
                {
                    settings.roles.eja = Role {
                        provider_id: provider.id.clone(),
                        model: primary,
                    };
                }
                if settings.roles.aja.provider_id.is_empty() && settings.roles.aja.model.is_empty()
                {
                    settings.roles.aja = Role {
                        provider_id: provider.id.clone(),
                        model: aja,
                    };
                }
                settings.providers = vec![provider];
            }
        }
        settings.limits = if settings.limits.slots == 0 {
            Limits {
                slots: 1,
                ..settings.limits
            }
        } else {
            settings.limits
        };
        Ok(settings)
    }

    pub fn write(&self, input: Value) -> ApiResult<StoredSettings> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| ApiError::internal("settings write lock is unavailable"))?;
        let current = self.read()?;
        let input: SaveInput = serde_json::from_value(input)?;
        let providers = normalize_providers(input.providers, &current.providers)?;
        let roles = normalize_roles(input.roles, &providers)?;
        let connectors =
            normalize_connectors(input.connectors.servers, &self.active(&current).servers)?;
        let workspace_tools = input
            .connectors
            .workspace_tools
            .unwrap_or(current.connectors.workspace_tools);
        let workspaces =
            normalize_workspaces(input.workspaces, self.project.as_deref(), &self.dir)?;
        let next = StoredSettings {
            providers,
            roles,
            connectors: ConnectorSettings {
                servers: connectors,
                managed: true,
                workspace_tools,
            },
            limits: input.limits.sane(),
            workspaces,
        };
        let raw = serde_json::to_value(&next)?;
        write_private_json(&self.path, &raw)?;
        self.write_managed_mcp(&next.connectors.servers)?;
        Ok(next)
    }

    pub fn prepare(&self) -> ApiResult<()> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| ApiError::internal("settings write lock is unavailable"))?;
        let settings = self.read()?;
        if settings.connectors.managed {
            self.write_managed_mcp(&settings.connectors.servers)?;
        }
        Ok(())
    }

    pub fn active(&self, settings: &StoredSettings) -> ActiveConnectors {
        if settings.connectors.managed {
            return ActiveConnectors {
                servers: settings.connectors.servers.clone(),
                managed: true,
                error: None,
                workspace_tools: settings.connectors.workspace_tools,
            };
        }
        let Some(path) = self.legacy_mcp_path() else {
            return ActiveConnectors {
                servers: settings.connectors.servers.clone(),
                managed: false,
                error: None,
                workspace_tools: settings.connectors.workspace_tools,
            };
        };
        match read_legacy_mcp(&path) {
            Ok(servers) => ActiveConnectors {
                servers: if settings.connectors.servers.is_empty() {
                    servers
                } else {
                    settings.connectors.servers.clone()
                },
                managed: false,
                error: None,
                workspace_tools: settings.connectors.workspace_tools,
            },
            Err(error) => ActiveConnectors {
                servers: settings.connectors.servers.clone(),
                managed: false,
                error: Some(format!("{}: {error}", path.display())),
                workspace_tools: settings.connectors.workspace_tools,
            },
        }
    }

    pub fn effective(&self, settings: StoredSettings) -> Effective {
        let connectors = self.active(&settings);
        let workspace_tools = connectors.workspace_tools;
        let eja = configured_role(&settings, "eja").or_else(|| environment_role(false));
        let aja = configured_role(&settings, "aja")
            .or_else(|| environment_role(true))
            .or_else(|| eja.clone());
        let mcp_path = if connectors.managed {
            Some(self.managed_mcp_path())
        } else {
            self.legacy_mcp_path().filter(|path| path.exists())
        };
        let mut limits = settings.limits.clone().sane();
        if let Some(value) = env_number("EJA_AJA_SLOTS").filter(|value| *value > 0) {
            limits.slots = value.min(8) as usize;
        }
        if let Some(value) = env_number("EJA_TURN_TIMEOUT_MS") {
            limits.turn_timeout_ms = value.min(7_200_000);
        }
        if let Some(value) = env_number("EJA_AJA_STEPS").filter(|value| *value > 0) {
            limits.aja_steps = value.min(200) as usize;
        }
        if let Some(value) = env_number("EJA_AJA_TIMEOUT_MS").filter(|value| *value > 0) {
            limits.aja_timeout_ms = value.min(14_400_000);
        }
        Effective {
            settings,
            connectors,
            roles: EffectiveRoles { eja, aja },
            mcp_path,
            limits,
            workspace_tools,
        }
    }

    pub fn view(&self, effective: &Effective) -> SettingsView {
        let settings = &effective.settings;
        SettingsView {
            providers: settings.providers.iter().map(public_provider).collect(),
            roles: settings.roles.clone(),
            connectors: ConnectorsView {
                servers: std::iter::once(builtin_workspace_view(
                    effective.connectors.workspace_tools,
                ))
                .chain(effective.connectors.servers.iter().map(public_connector))
                .collect(),
                managed: effective.connectors.managed,
                config_path: effective
                    .mcp_path
                    .as_ref()
                    .map_or_else(String::new, |path| path.display().to_string()),
                error: effective.connectors.error.clone(),
            },
            limits: settings.limits.clone(),
            workspaces: settings.workspaces.clone(),
            runtime_path: env::current_exe()
                .map_or_else(|_| "eja".to_owned(), |path| path.display().to_string()),
            state_dir: self.dir.display().to_string(),
            project_dir: self
                .project
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
        }
    }

    pub fn effective_current(&self) -> ApiResult<Effective> {
        Ok(self.effective(self.read()?))
    }

    /// The directories EJA may use. Registration is the durable availability
    /// decision; a missing directory is not offered and cannot be selected.
    pub fn grant(&self, settings: &StoredSettings) -> Vec<WorkspaceGrant> {
        let mut grants = Vec::new();
        if let Some(project) = &self.project {
            let root = project.display().to_string();
            grants.push(WorkspaceGrant {
                id: workspace_id(&root),
                label: "default".to_owned(),
                root,
            });
        }
        for (label, root) in &settings.workspaces {
            let Ok(path) = Path::new(root).canonicalize() else {
                continue;
            };
            if !path.is_dir()
                || self
                    .project
                    .as_ref()
                    .is_some_and(|project| path == *project)
            {
                continue;
            }
            let root = path.display().to_string();
            if grants.iter().any(|grant| grant.root == root) {
                continue;
            }
            grants.push(WorkspaceGrant {
                id: workspace_id(&root),
                label: label.clone(),
                root,
            });
        }
        grants
    }

    pub fn connector_for_test(&self, body: &Value) -> ApiResult<StoredConnector> {
        let current = self.read()?;
        let active = self.active(&current);
        let incoming = body.get("connector").unwrap_or(body);
        let name = incoming.get("name").and_then(Value::as_str).unwrap_or("");
        let previous = active
            .servers
            .iter()
            .find(|connector| connector.name == name);
        let input: ConnectorInput = serde_json::from_value(incoming.clone())?;
        normalize_connector(input, previous)
    }

    pub fn managed_mcp_path(&self) -> PathBuf {
        self.dir.join("app-mcp.json")
    }

    fn legacy_mcp_path(&self) -> Option<PathBuf> {
        let raw: Value = fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| json!({}));
        if let Some(path) = raw
            .get("connectors")
            .and_then(|connectors| connectors.get("mcpConfig"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            return Some(self.project.as_ref().unwrap_or(&self.dir).join(path));
        }
        let local = self.dir.join("mcp.json");
        local.is_file().then_some(local)
    }

    fn write_managed_mcp(&self, connectors: &[StoredConnector]) -> ApiResult<()> {
        let mut servers = serde_json::Map::new();
        for connector in connectors {
            servers.insert(connector.name.clone(), connector_config(connector));
        }
        write_private_json(&self.managed_mcp_path(), &json!({ "mcpServers": servers }))
    }
}

fn legacy_provider(raw: &Value) -> Option<Provider> {
    let model = raw.get("model")?;
    let base_url = model
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let api_key = model
        .get("apiKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let primary = model
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let aja = model
        .get("ajaModel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if base_url.is_empty() && api_key.is_empty() && primary.is_empty() && aja.is_empty() {
        return None;
    }
    Some(Provider {
        id: "default".to_owned(),
        name: provider_name(&base_url, "Provider"),
        base_url,
        api_key,
        models: unique_strings(Some(&vec![primary, aja])),
        headers: BTreeMap::new(),
    })
}

fn normalize_providers(
    values: Vec<ProviderInput>,
    previous: &[Provider],
) -> ApiResult<Vec<Provider>> {
    if values.len() > 16 {
        return Err(ApiError::bad("最多可以配置 16 个 Provider"));
    }
    let prior: BTreeMap<_, _> = previous
        .iter()
        .map(|provider| (provider.id.clone(), provider))
        .collect();
    let mut seen_ids = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    let mut output = Vec::new();
    for value in values {
        let mut id = value.id.trim().to_owned();
        if id.is_empty() {
            id = format!("p_{}", super::util::now_seconds());
        }
        if !valid_name(&id, 48) {
            return Err(ApiError::bad("Provider ID 无效"));
        }
        if !seen_ids.insert(id.clone()) {
            return Err(ApiError::bad("Provider ID 重复"));
        }
        let previous = prior.get(&id).copied();
        let base_url = normalize_base_url(&value.base_url)?;
        let api_key = if value.clear_api_key {
            String::new()
        } else if value.api_key.trim().is_empty() {
            previous.map_or_else(String::new, |provider| provider.api_key.clone())
        } else {
            value.api_key.trim().to_owned()
        };
        let name = if value.name.trim().is_empty() {
            provider_name(&base_url, &id)
        } else {
            value.name.trim().chars().take(48).collect()
        };
        if !seen_names.insert(name.to_lowercase()) {
            return Err(ApiError::bad("Provider 名称重复"));
        }
        let headers = normalize_key_values(
            value.headers,
            previous.map_or(&BTreeMap::new(), |provider| &provider.headers),
        )?;
        for key in headers.keys() {
            if matches!(
                key.to_ascii_lowercase().as_str(),
                "connection" | "content-length" | "content-type" | "host" | "transfer-encoding"
            ) {
                return Err(ApiError::bad(format!(
                    "Provider Header 由客户端管理：{key}"
                )));
            }
            validate_header(key)?;
        }
        output.push(Provider {
            id,
            name,
            base_url,
            api_key,
            models: unique_strings(Some(&value.models)),
            headers,
        });
    }
    Ok(output)
}

fn normalize_roles(input: Roles, providers: &[Provider]) -> ApiResult<Roles> {
    let ids: BTreeSet<_> = providers
        .iter()
        .map(|provider| provider.id.as_str())
        .collect();
    for (name, role) in [("eja", &input.eja), ("aja", &input.aja)] {
        if !role.provider_id.is_empty() && !ids.contains(role.provider_id.as_str()) {
            return Err(ApiError::bad(format!("{name} 选择了一个不存在的 Provider")));
        }
    }
    Ok(input)
}

/// A workspace ID follows the resolved directory, never its display label.
fn workspace_id(root: &str) -> String {
    let text = Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(root))
        .display()
        .to_string();
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("ws_{hash:016x}")
}

fn normalize_workspaces(
    input: BTreeMap<String, String>,
    project: Option<&Path>,
    dir: &Path,
) -> ApiResult<BTreeMap<String, String>> {
    if input.len() > 16 {
        return Err(ApiError::bad("最多可以配置 16 个工作区"));
    }
    let mut output = BTreeMap::new();
    for (name, root) in input {
        let name = name.trim().to_owned();
        if !valid_name(&name, 32) || name == "default" {
            return Err(ApiError::bad(
                "工作区名称需为 1–32 个字母、数字、下划线或短横线，且不能是 default",
            ));
        }
        let root = root.trim();
        if root.is_empty() {
            return Err(ApiError::bad(format!("工作区 {name} 需要一个目录路径")));
        }
        let path = Path::new(root)
            .canonicalize()
            .map_err(|_| ApiError::bad(format!("工作区 {name} 的目录不存在：{root}")))?;
        if !path.is_dir() {
            return Err(ApiError::bad(format!("工作区 {name} 的路径不是目录")));
        }
        if path.starts_with(dir) || dir.starts_with(&path) {
            return Err(ApiError::bad(format!("工作区 {name} 不能与状态目录重叠")));
        }
        if project.is_some_and(|project| {
            path == project
                .canonicalize()
                .unwrap_or_else(|_| project.to_path_buf())
        }) {
            return Err(ApiError::bad(format!(
                "工作区 {name} 就是默认工作区，无需另加"
            )));
        }
        let root = path.display().to_string();
        if output.values().any(|existing| existing == &root) {
            return Err(ApiError::bad(format!(
                "工作区 {name} 的目录已被另一个工作区使用"
            )));
        }
        output.insert(name, root);
    }
    Ok(output)
}

fn normalize_connectors(
    values: Vec<ConnectorInput>,
    previous: &[StoredConnector],
) -> ApiResult<Vec<StoredConnector>> {
    if values.len() > 32 {
        return Err(ApiError::bad("最多可以配置 32 个连接器"));
    }
    if values
        .iter()
        .any(|value| value.name.trim() == eja::adapters::WORKSPACE_GROUP)
    {
        return Err(ApiError::bad("workspace 是内置连接器，不能另建同名连接器"));
    }
    let prior: BTreeMap<_, _> = previous
        .iter()
        .map(|connector| (connector.name.clone(), connector))
        .collect();
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for value in values {
        let previous = prior.get(value.name.trim()).copied();
        let connector = normalize_connector(value, previous)?;
        if !seen.insert(connector.name.clone()) {
            return Err(ApiError::bad(format!("连接器名称重复：{}", connector.name)));
        }
        output.push(connector);
    }
    Ok(output)
}

fn normalize_connector(
    value: ConnectorInput,
    previous: Option<&StoredConnector>,
) -> ApiResult<StoredConnector> {
    let name = value.name.trim().to_owned();
    if !valid_name(&name, 24) {
        return Err(ApiError::bad(
            "连接器名称需为 1–24 个字母、数字、下划线或短横线",
        ));
    }
    let url_mode = value.mode == "url" || !value.url.trim().is_empty();
    let command = if url_mode {
        String::new()
    } else {
        value.command.trim().to_owned()
    };
    let url = if url_mode {
        value.url.trim().to_owned()
    } else {
        String::new()
    };
    if command.is_empty() && url.is_empty() {
        return Err(ApiError::bad(format!("{name} 需要本地命令或远程 URL")));
    }
    if !url.is_empty() {
        let parsed =
            reqwest::Url::parse(&url).map_err(|_| ApiError::bad(format!("{name} 的 URL 无效")))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(ApiError::bad(format!(
                "{name} 需要不含账号信息的 HTTP(S) URL"
            )));
        }
    }
    let env = if url_mode {
        BTreeMap::new()
    } else {
        normalize_key_values(
            value.env,
            previous.map_or(&BTreeMap::new(), |connector| &connector.env),
        )?
    };
    for key in env.keys() {
        let mut chars = key.chars();
        if !chars
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
            || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            return Err(ApiError::bad(format!("环境变量名无效：{key}")));
        }
    }
    let headers = if url_mode {
        normalize_key_values(
            value.headers,
            previous.map_or(&BTreeMap::new(), |connector| &connector.headers),
        )?
    } else {
        BTreeMap::new()
    };
    for key in headers.keys() {
        validate_header(key)?;
    }
    Ok(StoredConnector {
        name,
        command: (!command.is_empty()).then_some(command),
        url: (!url.is_empty()).then_some(url),
        args: if url_mode { Vec::new() } else { value.args },
        env,
        cwd: if url_mode {
            String::new()
        } else {
            value.cwd.trim().to_owned()
        },
        headers,
        enabled: value.enabled,
        timeout_ms: value.timeout_ms.clamp(1, 300_000),
    })
}

fn normalize_key_values(
    values: Vec<KeyValueInput>,
    previous: &BTreeMap<String, String>,
) -> ApiResult<BTreeMap<String, String>> {
    let mut output = BTreeMap::new();
    for value in values {
        let key = value.key.trim();
        if key.is_empty() {
            continue;
        }
        if value.redacted && value.value.is_empty() {
            if let Some(previous) = previous.get(key) {
                output.insert(key.to_owned(), previous.clone());
            }
            continue;
        }
        if value.value.contains(['\r', '\n']) {
            return Err(ApiError::bad(format!("值不能包含换行：{key}")));
        }
        output.insert(key.to_owned(), value.value);
    }
    Ok(output)
}

fn validate_header(key: &str) -> ApiResult<()> {
    if key.is_empty()
        || !key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
    {
        return Err(ApiError::bad(format!("Header 名称无效：{key}")));
    }
    Ok(())
}

fn valid_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn configured_role(settings: &StoredSettings, name: &str) -> Option<RoleConfig> {
    let role = if name == "eja" {
        &settings.roles.eja
    } else {
        &settings.roles.aja
    };
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == role.provider_id)?;
    Some(RoleConfig {
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        model: role.model.clone(),
        headers: provider.headers.clone(),
    })
}

fn environment_role(aja: bool) -> Option<RoleConfig> {
    let base_url = if aja {
        env_value(&["EJA_AJA_BASE_URL"])
    } else {
        String::new()
    };
    let api_key = if aja {
        env_value(&["EJA_AJA_API_KEY"])
    } else {
        String::new()
    };
    let model = if aja {
        env_value(&["EJA_AJA_MODEL"])
    } else {
        String::new()
    };
    let base_url = if base_url.is_empty() {
        env_value(&["LNA_BASE_URL", "OPENAI_BASE_URL", "JIYUAN_BASE_URL"])
    } else {
        base_url
    };
    let api_key = if api_key.is_empty() {
        env_value(&["LNA_API_KEY", "OPENAI_API_KEY", "JIYUAN_API_KEY"])
    } else {
        api_key
    };
    let model = if model.is_empty() {
        env_value(&["LNA_MODEL", "OPENAI_MODEL", "JIYUAN_MODEL"])
    } else {
        model
    };
    if base_url.is_empty() && api_key.is_empty() && model.is_empty() {
        return None;
    }
    Some(RoleConfig {
        base_url,
        api_key,
        model,
        headers: env_headers(if aja {
            "EJA_AJA_HEADERS"
        } else {
            "LNA_HEADERS"
        }),
    })
}

fn env_value(names: &[&str]) -> String {
    names
        .iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_default()
}

fn env_number(name: &str) -> Option<u64> {
    env::var(name).ok().and_then(|value| value.parse().ok())
}

fn env_headers(name: &str) -> BTreeMap<String, String> {
    env::var(name)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn public_provider(provider: &Provider) -> ProviderView {
    ProviderView {
        id: provider.id.clone(),
        name: provider.name.clone(),
        base_url: provider.base_url.clone(),
        models: provider.models.clone(),
        has_api_key: !provider.api_key.is_empty(),
        local: is_local_provider(&provider.base_url),
        headers: public_key_values(&provider.headers),
    }
}

fn public_connector(connector: &StoredConnector) -> ConnectorView {
    ConnectorView {
        name: connector.name.clone(),
        mode: if connector.command.is_some() {
            "command"
        } else {
            "url"
        },
        builtin: false,
        command: connector.command.clone().unwrap_or_default(),
        url: connector.url.clone().unwrap_or_default(),
        args: connector.args.clone(),
        cwd: connector.cwd.clone(),
        env: public_key_values(&connector.env),
        headers: public_key_values(&connector.headers),
        enabled: connector.enabled,
        timeout_ms: connector.timeout_ms,
    }
}

/// The built-in workspace connector as the settings list shows it: one row,
/// switchable, with nothing to edit and nothing to delete.
fn builtin_workspace_view(enabled: bool) -> ConnectorView {
    ConnectorView {
        name: eja::adapters::WORKSPACE_GROUP.to_owned(),
        mode: "command",
        builtin: true,
        command: String::new(),
        url: String::new(),
        args: Vec::new(),
        cwd: String::new(),
        env: Vec::new(),
        headers: Vec::new(),
        enabled,
        timeout_ms: 0,
    }
}

fn public_key_values(values: &BTreeMap<String, String>) -> Vec<KeyValueView> {
    values
        .iter()
        .map(|(key, value)| {
            let reference = value.starts_with("${")
                && value.ends_with('}')
                && value[2..value.len().saturating_sub(1)]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            let secret = is_secret(key);
            let redacted = secret && !value.is_empty() && !reference;
            KeyValueView {
                key: key.clone(),
                value: if redacted {
                    String::new()
                } else {
                    value.clone()
                },
                secret,
                has_value: !value.is_empty(),
                redacted,
            }
        })
        .collect()
}

fn is_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "secret",
        "token",
        "key",
        "pass",
        "auth",
        "credential",
        "cookie",
        "session",
    ]
    .iter()
    .any(|part| lower.contains(part))
}

pub fn connector_config(connector: &StoredConnector) -> Value {
    let mut config = serde_json::Map::new();
    if let Some(command) = &connector.command {
        config.insert("command".to_owned(), json!(command));
        config.insert("args".to_owned(), json!(connector.args));
        if !connector.env.is_empty() {
            config.insert("env".to_owned(), json!(connector.env));
        }
        if !connector.cwd.is_empty() {
            config.insert("cwd".to_owned(), json!(connector.cwd));
        }
    } else {
        config.insert("url".to_owned(), json!(connector.url));
        if !connector.headers.is_empty() {
            config.insert("headers".to_owned(), json!(connector.headers));
        }
    }
    config.insert("enabled".to_owned(), json!(connector.enabled));
    config.insert("timeout_ms".to_owned(), json!(connector.timeout_ms));
    Value::Object(config)
}

fn read_legacy_mcp(path: &Path) -> ApiResult<Vec<StoredConnector>> {
    let raw: Value = serde_json::from_slice(&fs::read(path)?)?;
    let servers = raw
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::bad("mcpServers must be an object"))?;
    let mut output = Vec::new();
    for (name, value) in servers {
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let url = value
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if command.is_none() && url.is_none() {
            return Err(ApiError::bad(format!("{name} needs command or url")));
        }
        output.push(StoredConnector {
            name: name.clone(),
            command,
            url,
            args: value
                .get("args")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            env: value
                .get("env")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default(),
            cwd: value
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            headers: value
                .get("headers")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default(),
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            timeout_ms: value
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(30_000)
                .clamp(1, 300_000),
        });
    }
    Ok(output)
}

pub fn provider_draft(
    body: &Value,
    current: &StoredSettings,
) -> ApiResult<(String, String, BTreeMap<String, String>)> {
    let id = body.get("id").and_then(Value::as_str).unwrap_or_default();
    let stored = current.providers.iter().find(|provider| provider.id == id);
    let base_url = body
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| stored.map(|provider| provider.base_url.clone()))
        .unwrap_or_default();
    let clear = body
        .get("clearApiKey")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let api_key = if clear {
        String::new()
    } else {
        body.get("apiKey")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| stored.map(|provider| provider.api_key.clone()))
            .unwrap_or_default()
    };
    let headers = body
        .get("headers")
        .cloned()
        .map(serde_json::from_value::<Vec<KeyValueInput>>)
        .transpose()?
        .map(|values| {
            normalize_key_values(
                values,
                stored.map_or(&BTreeMap::new(), |provider| &provider.headers),
            )
        })
        .transpose()?
        .unwrap_or_else(|| stored.map_or_else(BTreeMap::new, |provider| provider.headers.clone()));
    Ok((base_url, api_key, headers))
}

#[cfg(test)]
mod tests {
    use super::{SettingsStore, StoredSettings};
    use serde_json::json;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eja-host-settings-{}-{}-{}",
            std::process::id(),
            super::super::util::now_seconds(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("app")).unwrap();
        path
    }

    #[test]
    fn legacy_settings_move_to_private_state_and_redacted_saves_keep_secrets() {
        let root = root();
        let project = root.join("project");
        let state = project.join(".eja");
        std::fs::create_dir_all(project.join("app")).unwrap();
        std::fs::write(
            project.join("app/settings.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": [{
                    "id": "p1",
                    "name": "Example",
                    "baseUrl": "https://example.com/v1",
                    "apiKey": "secret-key-value",
                    "models": ["m1"],
                    "headers": {"x-session-token": "session-value"}
                }],
                "roles": {
                    "eja": {"providerId": "p1", "model": "m1"},
                    "aja": {"providerId": "p1", "model": "m1"}
                },
                "connectors": {"servers": [], "managed": true},
                "limits": {"slots": 1, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0}
            }))
            .unwrap(),
        )
        .unwrap();
        let store = SettingsStore::new(Some(project), state.clone());
        let settings = store.read().unwrap();
        assert_eq!(settings.providers[0].api_key, "secret-key-value");
        assert!(state.join("app-settings.json").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(state.join("app-settings.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let next = json!({
            "providers": [{
                "id": "p1",
                "name": "Example",
                "baseUrl": "https://example.com/v1",
                "models": ["m1"],
                "headers": [{"key": "x-session-token", "value": "", "redacted": true}]
            }],
            "roles": {
                "eja": {"providerId": "p1", "model": "m1"},
                "aja": {"providerId": "p1", "model": "m1"}
            },
            "connectors": {"servers": []},
            "limits": {"slots": 1, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0}
        });
        let saved = store.write(next).unwrap();
        assert_eq!(saved.providers[0].api_key, "secret-key-value");
        assert_eq!(
            saved.providers[0].headers.get("x-session-token"),
            Some(&"session-value".to_owned())
        );
        let view = serde_json::to_string(&store.view(&store.effective(saved))).unwrap();
        assert!(!view.contains("secret-key-value"));
        assert!(!view.contains("session-value"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_fresh_state_gets_safe_defaults() {
        let root = root();
        let store = SettingsStore::new(Some(root.join("project")), root.join(".eja"));
        let settings: StoredSettings = store.read().unwrap();
        assert_eq!(settings.limits.slots, 1);
        assert!(settings.providers.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_builtin_workspace_connector_is_switchable_and_reserved() {
        let root = root();
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let store = SettingsStore::new(Some(project), root.join(".eja"));
        let effective = store.effective_current().unwrap();
        assert!(effective.workspace_tools, "on by default");
        let view = store.view(&effective);
        assert_eq!(view.connectors.servers[0].name, "workspace");
        assert!(view.connectors.servers[0].builtin);
        assert!(view.connectors.servers[0].enabled);

        let base = json!({
            "providers": [],
            "roles": {"eja": {"providerId": "", "model": ""}, "aja": {"providerId": "", "model": ""}},
            "limits": {"slots": 1, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0}
        });
        let mut off = base.clone();
        off["connectors"] = json!({"servers": [], "workspaceTools": false});
        let saved = store.write(off).unwrap();
        assert!(!saved.connectors.workspace_tools);
        assert!(
            saved.connectors.servers.is_empty(),
            "the builtin is not a stored server"
        );

        // A save that does not mention the switch keeps it.
        let mut keep = base.clone();
        keep["connectors"] = json!({"servers": []});
        let saved = store.write(keep).unwrap();
        assert!(!saved.connectors.workspace_tools);

        let mut clash = base;
        clash["connectors"] = json!({"servers": [{"name": "workspace", "command": "x"}]});
        assert!(store.write(clash).is_err(), "the name is reserved");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stored_extra_directories_are_granted_until_they_disappear() {
        let root = root();
        let project = root.join("project");
        let extra = root.join("extra");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        let store = SettingsStore::new(Some(project.clone()), root.join(".eja"));
        let mut settings = StoredSettings::default();
        settings
            .workspaces
            .insert("old".into(), extra.display().to_string());

        let grant = store.grant(&settings);
        assert_eq!(grant.len(), 2);
        assert_eq!(grant[0].label, "default");
        assert_eq!(grant[0].root, project.display().to_string());
        assert!(grant[0].id.starts_with("ws_"));
        assert_eq!(grant[1].label, "old");
        assert_eq!(
            grant[1].root,
            extra.canonicalize().unwrap().display().to_string()
        );

        let again = store.grant(&settings);
        assert_eq!(grant[0].id, again[0].id);
        assert_eq!(grant[1].id, again[1].id);

        std::fs::remove_dir_all(&extra).unwrap();
        assert_eq!(store.grant(&settings).len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
