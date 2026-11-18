//! MCP connector configuration behind the generic capability card.

use super::{
    settings::{
        normalize_key_values, public_key_values, valid_name, validate_header, KeyValueInput,
        KeyValueView,
    },
    util::{ApiError, ApiResult},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub read_only: Vec<String>,
    #[serde(default)]
    pub witness: Vec<String>,
}

const fn default_true() -> bool {
    true
}

const fn default_timeout() -> u64 {
    30_000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorSettings {
    #[serde(default)]
    pub servers: Vec<StoredConnector>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ConnectorInput {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: Vec<KeyValueInput>,
    #[serde(default)]
    pub headers: Vec<KeyValueInput>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub read_only: Vec<String>,
    #[serde(default)]
    pub witness: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ConnectorSaveInput {
    #[serde(default)]
    pub servers: Vec<ConnectorInput>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorView {
    pub name: String,
    pub mode: &'static str,
    pub command: String,
    pub url: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<KeyValueView>,
    pub headers: Vec<KeyValueView>,
    pub enabled: bool,
    pub timeout_ms: u64,
    pub tools: Vec<String>,
    pub read_only: Vec<String>,
    pub witness: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorsView {
    pub servers: Vec<ConnectorView>,
}

pub(super) fn normalize(
    values: Vec<ConnectorInput>,
    previous: &[StoredConnector],
) -> ApiResult<Vec<StoredConnector>> {
    if values.len() > 32 {
        return Err(ApiError::bad("最多可以配置 32 个连接器"));
    }
    let prior: BTreeMap<_, _> = previous
        .iter()
        .map(|connector| (connector.name.clone(), connector))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    for value in values {
        let previous = prior.get(value.name.trim()).copied();
        let connector = normalize_one(value, previous)?;
        if !seen.insert(connector.name.clone()) {
            return Err(ApiError::bad(format!("连接器名称重复：{}", connector.name)));
        }
        output.push(connector);
    }
    Ok(output)
}

pub(super) fn draft(body: &Value, previous: &[StoredConnector]) -> ApiResult<StoredConnector> {
    let incoming = body.get("connector").unwrap_or(body);
    let name = incoming.get("name").and_then(Value::as_str).unwrap_or("");
    let previous = previous.iter().find(|connector| connector.name == name);
    let input: ConnectorInput = serde_json::from_value(incoming.clone())?;
    normalize_one(input, previous)
}

pub(super) fn public_value(settings: &ConnectorSettings) -> Value {
    serde_json::to_value(ConnectorsView {
        servers: settings.servers.iter().map(public_connector).collect(),
    })
    .unwrap_or_else(|_| json!({"servers":[]}))
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
    if !connector.tools.is_empty() {
        config.insert("tools".to_owned(), json!(connector.tools));
    }
    if !connector.read_only.is_empty() {
        config.insert("read_only".to_owned(), json!(connector.read_only));
    }
    if !connector.witness.is_empty() {
        config.insert("witness".to_owned(), json!(connector.witness));
    }
    Value::Object(config)
}

fn normalize_one(
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
        tools: super::util::unique_strings(Some(&value.tools)),
        read_only: super::util::unique_strings(Some(&value.read_only)),
        witness: super::util::unique_strings(Some(&value.witness)),
    })
}

fn public_connector(connector: &StoredConnector) -> ConnectorView {
    ConnectorView {
        name: connector.name.clone(),
        mode: if connector.command.is_some() {
            "command"
        } else {
            "url"
        },
        command: connector.command.clone().unwrap_or_default(),
        url: connector.url.clone().unwrap_or_default(),
        args: connector.args.clone(),
        cwd: connector.cwd.clone(),
        env: public_key_values(&connector.env),
        headers: public_key_values(&connector.headers),
        enabled: connector.enabled,
        timeout_ms: connector.timeout_ms,
        tools: connector.tools.clone(),
        read_only: connector.read_only.clone(),
        witness: connector.witness.clone(),
    }
}
