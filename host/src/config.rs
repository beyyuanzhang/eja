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
        let base = self.base_url.trim();
        if base.is_empty() {
            return false;
        }
        if !self.api_key.trim().is_empty() {
            return true;
        }
        base.contains("127.0.0.1") || base.contains("localhost")
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
        }
    }
}

impl HostConfig {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    pub fn load(data_dir: &Path) -> Self {
        let cfg_path = Self::path(data_dir);
        if let Ok(bytes) = fs::read(&cfg_path) {
            if let Ok(cfg) = serde_json::from_slice::<Self>(&bytes) {
                return cfg;
            }
        }
        Self::default()
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        let path = Self::path(data_dir);
        let tmp = data_dir.join("config.json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(|err| err.to_string())?;
        fs::write(&tmp, bytes).map_err(|err| err.to_string())?;
        fs::rename(&tmp, &path).map_err(|err| err.to_string())
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
}
