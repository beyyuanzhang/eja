use super::{
    capability::{
        self, CapabilityDescriptor, CapabilitySetting, CapabilitySettings, CapabilityView, MCP,
    },
    connectors::{self, ConnectorSaveInput, ConnectorSettings, StoredConnector},
    plugins,
    util::{
        is_local_provider, normalize_base_url, provider_name, unique_strings, write_private_json,
        ApiError, ApiResult,
    },
};
use eja::WorkspaceGrant;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Role {
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    #[serde(default)]
    pub eja: Role,
    #[serde(default)]
    pub aja: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub slots: usize,
    pub turn_timeout_ms: u64,
    pub aja_steps: usize,
    pub aja_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            slots: 1,
            turn_timeout_ms: 0,
            aja_steps: 0,
            aja_timeout_ms: 0,
        }
    }
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredSettings {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub roles: Roles,
    #[serde(default)]
    pub capabilities: CapabilitySettings,
    pub limits: Limits,
    /// Extra directories the user has made available to EJA. A task fixes one
    /// of these roots when it opens.
    #[serde(default)]
    pub workspaces: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub(super) struct KeyValueInput {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub redacted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveInput {
    #[serde(default)]
    providers: Vec<ProviderInput>,
    #[serde(default)]
    roles: Roles,
    #[serde(default)]
    capabilities: BTreeMap<String, CapabilityInput>,
    limits: Limits,
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CapabilityInput {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "super::capability::empty_object")]
    config: Value,
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
pub struct SettingsView {
    pub providers: Vec<ProviderView>,
    pub roles: Roles,
    pub capabilities: Vec<CapabilityView>,
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
pub struct Effective {
    pub settings: StoredSettings,
    pub connectors: Vec<StoredConnector>,
    pub roles: EffectiveRoles,
    pub catalog: Vec<CapabilityDescriptor>,
    pub limits: Limits,
}

impl Effective {
    pub fn model_ready(&self) -> bool {
        self.roles.eja.as_ref().is_some_and(RoleConfig::configured)
            && self.roles.aja.as_ref().is_some_and(RoleConfig::configured)
    }

    pub fn mcp_ready(&self) -> bool {
        self.settings.capabilities.enabled(MCP)
            && self.connectors.iter().any(|server| server.enabled)
    }

    pub fn managed_code_enabled(&self) -> bool {
        self.settings
            .capabilities
            .enabled(capability::MANAGED_PLUGINS)
            || self.catalog.iter().any(|descriptor| {
                descriptor.source == "plugin" && self.settings.capabilities.enabled(&descriptor.id)
            })
    }

    pub fn grants(&self, settings: &SettingsStore) -> Vec<WorkspaceGrant> {
        if self
            .settings
            .capabilities
            .enabled(capability::LOCAL_WORKSPACE)
        {
            settings.grant(&self.settings)
        } else {
            Vec::new()
        }
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
            return Ok(StoredSettings::default());
        }
        Ok(serde_json::from_slice(&fs::read(&self.path)?)?)
    }

    pub fn state_dir(&self) -> &Path {
        &self.dir
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
        let workspaces =
            normalize_workspaces(input.workspaces, self.project.as_deref(), &self.dir)?;
        let catalog = self.catalog();
        let capabilities =
            Self::normalize_capabilities(input.capabilities, &current.capabilities, &catalog)?;
        let next = StoredSettings {
            providers,
            roles,
            capabilities,
            limits: input.limits.sane(),
            workspaces,
        };
        let raw = serde_json::to_value(&next)?;
        write_private_json(&self.path, &raw)?;
        Ok(next)
    }

    fn effective_with_catalog(
        mut settings: StoredSettings,
        catalog: Vec<CapabilityDescriptor>,
    ) -> ApiResult<Effective> {
        settings.capabilities = settings.capabilities.normalize(&catalog);
        let connectors = mcp_config(&settings.capabilities)?.servers;
        let eja = configured_role(&settings, "eja");
        let aja = configured_role(&settings, "aja").or_else(|| eja.clone());
        let limits = settings.limits.clone().sane();
        Ok(Effective {
            settings,
            connectors,
            roles: EffectiveRoles { eja, aja },
            catalog,
            limits,
        })
    }

    pub fn view(&self, effective: &Effective) -> SettingsView {
        let settings = &effective.settings;
        SettingsView {
            providers: settings.providers.iter().map(public_provider).collect(),
            roles: settings.roles.clone(),
            capabilities: capability::views(&effective.catalog, &settings.capabilities)
                .into_iter()
                .map(|mut view| {
                    if view.id == MCP {
                        if let Ok(config) = mcp_config(&settings.capabilities) {
                            view.config = connectors::public_value(&config);
                        }
                    }
                    view
                })
                .collect(),
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
        Self::effective_with_catalog(self.read()?, self.catalog())
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

    fn catalog(&self) -> Vec<CapabilityDescriptor> {
        let mut catalog = capability::builtins();
        catalog.extend(plugins::catalog(&self.dir));
        catalog
    }

    fn normalize_capabilities(
        input: BTreeMap<String, CapabilityInput>,
        previous: &CapabilitySettings,
        catalog: &[CapabilityDescriptor],
    ) -> ApiResult<CapabilitySettings> {
        let mut entries = previous.entries().clone();
        for (id, capability) in input {
            let id = id.trim();
            if !valid_capability_id(id) {
                return Err(ApiError::bad("能力 ID 无效"));
            }
            if !entries.contains_key(id) && !catalog.iter().any(|item| item.id == id) {
                return Err(ApiError::bad(format!("能力 {id} 不在当前能力目录中")));
            }
            if !entries.contains_key(id) && entries.len() >= 128 {
                return Err(ApiError::bad("最多可以配置 128 个能力"));
            }
            let mut config = capability.config;
            if id == MCP {
                let previous = mcp_config(previous)?;
                let input: ConnectorSaveInput = serde_json::from_value(config)?;
                let servers = connectors::normalize(input.servers, &previous.servers)?;
                config = serde_json::to_value(ConnectorSettings { servers })?;
            }
            let config_limit = if id.starts_with("plugin:") {
                64 * 1024
            } else {
                512 * 1024
            };
            if serde_json::to_vec(&config)?.len() > config_limit {
                return Err(ApiError::bad(format!(
                    "能力 {id} 的配置超过 {} KiB",
                    config_limit / 1024
                )));
            }
            entries.insert(
                id.to_owned(),
                CapabilitySetting::new(capability.enabled, config),
            );
        }
        let mut capabilities = CapabilitySettings::from_entries(entries);
        for descriptor in catalog {
            capabilities.ensure(descriptor);
        }
        Ok(capabilities)
    }

    pub fn connector_for_test(&self, body: &Value) -> ApiResult<StoredConnector> {
        let current = self.read()?;
        let previous_config = mcp_config(&current.capabilities)?;
        connectors::draft(body, &previous_config.servers)
    }
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

fn valid_capability_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b':' | b'.')
        })
}

fn mcp_config(capabilities: &CapabilitySettings) -> ApiResult<ConnectorSettings> {
    Ok(serde_json::from_value(capabilities.config(MCP))?)
}

pub(super) fn normalize_key_values(
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

pub(super) fn validate_header(key: &str) -> ApiResult<()> {
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

pub(super) fn valid_name(value: &str, max: usize) -> bool {
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

pub(super) fn public_key_values(values: &BTreeMap<String, String>) -> Vec<KeyValueView> {
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

pub(super) fn is_secret(key: &str) -> bool {
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
    fn settings_round_trip_and_redacted_saves_keep_secrets() {
        let root = root();
        let project = root.join("project");
        let state = project.join(".eja");
        std::fs::create_dir_all(&project).unwrap();
        let store = SettingsStore::new(Some(project), state.clone());
        let settings = store
            .write(json!({
                "providers": [{
                    "id": "p1",
                    "name": "Example",
                    "baseUrl": "https://example.com/v1",
                    "apiKey": "secret-key-value",
                    "models": ["m1"],
                    "headers": [{"key": "x-session-token", "value": "session-value"}]
                }],
                "roles": {
                    "eja": {"providerId": "p1", "model": "m1"},
                    "aja": {"providerId": "p1", "model": "m1"}
                },
                "capabilities": {
                    "local-workspace": {"enabled": true},
                    "files": {"enabled": true},
                    "shell": {"enabled": true},
                    "git-worktrees": {"enabled": true},
                    "checkpoints": {"enabled": true},
                    "mcp": {"enabled": true, "config": {"servers": []}},
                    "managed-plugins": {"enabled": false}
                },
                "limits": {"slots": 1, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0},
                "workspaces": {}
            }))
            .unwrap();
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
            "capabilities": {
                "local-workspace": {"enabled": true},
                "files": {"enabled": true},
                "shell": {"enabled": true},
                "git-worktrees": {"enabled": true},
                "checkpoints": {"enabled": true},
                "mcp": {"enabled": true, "config": {"servers": []}},
                "managed-plugins": {"enabled": false}
            },
            "limits": {"slots": 1, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0},
            "workspaces": {}
        });
        let saved = store.write(next).unwrap();
        assert_eq!(saved.providers[0].api_key, "secret-key-value");
        assert_eq!(
            saved.providers[0].headers.get("x-session-token"),
            Some(&"session-value".to_owned())
        );
        let effective = store.effective_current().unwrap();
        let view = serde_json::to_string(&store.view(&effective)).unwrap();
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
    fn capabilities_are_stored_in_the_same_settings_file() {
        let root = root();
        let store = SettingsStore::new(Some(root.join("project")), root.join(".eja"));
        let saved = store.write(json!({
            "providers": [],
            "roles": {"eja": {"providerId": "", "model": ""}, "aja": {"providerId": "", "model": ""}},
            "capabilities": {
                "local-workspace": {"enabled": true},
                "files": {"enabled": false},
                "shell": {"enabled": true},
                "git-worktrees": {"enabled": false},
                "checkpoints": {"enabled": true},
                "mcp": {"enabled": true, "config": {"servers": []}},
                "managed-plugins": {"enabled": false}
            },
            "limits": {"slots": 2, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0},
            "workspaces": {}
        })).unwrap();
        assert_eq!(
            serde_json::to_value(&saved).unwrap(),
            serde_json::to_value(store.read().unwrap()).unwrap()
        );
        assert!(saved.capabilities.enabled("local-workspace"));
        assert!(!saved.capabilities.enabled("files"));
        assert!(saved.capabilities.enabled("shell"));
        assert!(!saved.capabilities.enabled("git-worktrees"));
        assert!(saved.capabilities.enabled("checkpoints"));
        assert_eq!(saved.capabilities.config("mcp")["servers"], json!([]));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn settings_cannot_invent_a_capability_card() {
        let root = root();
        let store = SettingsStore::new(Some(root.join("project")), root.join(".eja"));
        let result = store.write(json!({
            "providers": [],
            "roles": {"eja": {"providerId": "", "model": ""}, "aja": {"providerId": "", "model": ""}},
            "capabilities": {
                "invented": {"enabled": true, "config": {}}
            },
            "limits": {"slots": 1, "turnTimeoutMs": 0, "ajaSteps": 0, "ajaTimeoutMs": 0},
            "workspaces": {}
        }));
        assert!(result.is_err());
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
