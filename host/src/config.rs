use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
}

impl ProviderConfig {
    pub fn is_usable(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
}

impl ModelRef {
    pub fn is_set(&self) -> bool {
        !self.provider_id.trim().is_empty() && !self.model.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectorTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub transport: ConnectorTransport,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub judge: ModelRef,
    #[serde(default)]
    pub connectors: Vec<ConnectorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_max_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<usize>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            providers: vec![ProviderConfig {
                id: "openai".into(),
                name: "OpenAI".into(),
                kind: ProviderKind::OpenAi,
                base_url: "https://api.openai.com/v1".into(),
                api_key: String::new(),
                models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            }],
            judge: ModelRef {
                provider_id: "openai".into(),
                model: "gpt-4o".into(),
            },
            connectors: Vec::new(),
            window_max_bytes: None,
            max_steps: None,
        }
    }
}

impl HostConfig {
    pub fn config_path(core_dir: &Path) -> PathBuf {
        core_dir.join("config.json")
    }

    pub fn connectors_path(data_dir: &Path) -> PathBuf {
        data_dir.join("connectors.json")
    }

    pub fn load(core_dir: &Path, data_dir: &Path) -> Self {
        let mut config = fs::read(Self::config_path(core_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default();
        config.connectors = Self::load_connectors(data_dir).unwrap_or_default();
        config
    }

    pub fn load_connectors(data_dir: &Path) -> Result<Vec<ConnectorConfig>, String> {
        let path = Self::connectors_path(data_dir);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("读取连接器配置失败: {err}")),
        };
        serde_json::from_slice::<Vec<ConnectorConfig>>(&bytes)
            .map_err(|err| format!("解析 connectors.json 失败: {err}"))
    }

    pub fn save(&self, core_dir: &Path, data_dir: &Path) -> Result<(), String> {
        let mut core_value = serde_json::to_value(self).map_err(|err| err.to_string())?;
        if let Some(map) = core_value.as_object_mut() {
            map.remove("connectors");
        }
        write_json_atomic(&Self::config_path(core_dir), &core_value)?;
        write_json_atomic(&Self::connectors_path(data_dir), &self.connectors)
    }

    pub fn resolve_judge(&self) -> Option<(&ProviderConfig, &str)> {
        let target = &self.judge;
        if !target.is_set() {
            return None;
        }
        let provider = self
            .providers
            .iter()
            .find(|item| item.id == target.provider_id)?;
        if !provider.is_usable() {
            return None;
        }
        Some((provider, target.model.trim()))
    }

    pub fn is_configured(&self) -> bool {
        self.resolve_judge().is_some()
    }

    pub fn window_max_bytes(&self) -> usize {
        self.window_max_bytes
            .filter(|&bytes| bytes > 0)
            .unwrap_or(crate::llm::WINDOW_MAX_BYTES)
    }

    pub fn max_steps(&self) -> usize {
        self.max_steps.filter(|&steps| steps > 0).unwrap_or(40)
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    fs::write(&tmp, bytes).map_err(|err| err.to_string())?;
    fs::rename(&tmp, path).map_err(|err| err.to_string())
}
