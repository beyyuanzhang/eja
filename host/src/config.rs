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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Assignments {
    #[serde(default)]
    pub judge: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidate: Option<ModelRef>,
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
    pub assignments: Assignments,
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
            assignments: Assignments {
                judge: ModelRef {
                    provider_id: "openai".into(),
                    model: "gpt-4o".into(),
                },
                consolidate: None,
            },
            connectors: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct LegacySettings {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    consolidate_model: String,
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
        let legacy_path = data_dir.join("settings.json");
        if let Ok(bytes) = fs::read(&legacy_path) {
            if let Ok(legacy) = serde_json::from_slice::<LegacySettings>(&bytes) {
                let model = if legacy.model.trim().is_empty() {
                    "gpt-4o".to_string()
                } else {
                    legacy.model.trim().to_string()
                };
                let mut models = vec![model.clone()];
                let consolidate = if !legacy.consolidate_model.trim().is_empty() {
                    let cm = legacy.consolidate_model.trim().to_string();
                    if !models.contains(&cm) {
                        models.push(cm.clone());
                    }
                    Some(ModelRef {
                        provider_id: "default".into(),
                        model: cm,
                    })
                } else {
                    None
                };
                let migrated = Self {
                    providers: vec![ProviderConfig {
                        id: "default".into(),
                        name: "Default".into(),
                        kind: ProviderKind::OpenAi,
                        base_url: if legacy.base_url.trim().is_empty() {
                            "https://api.openai.com/v1".into()
                        } else {
                            legacy.base_url.trim().to_string()
                        },
                        api_key: legacy.api_key.trim().to_string(),
                        models,
                    }],
                    assignments: Assignments {
                        judge: ModelRef {
                            provider_id: "default".into(),
                            model,
                        },
                        consolidate,
                    },
                    connectors: Vec::new(),
                };
                let _ = migrated.save(data_dir);
                return migrated;
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
        self.resolve_ref(&self.assignments.judge)
    }

    pub fn resolve_consolidate(&self) -> Option<(&ProviderConfig, &str)> {
        if let Some(target) = &self.assignments.consolidate {
            if let Some(resolved) = self.resolve_ref(target) {
                return Some(resolved);
            }
        }
        self.resolve_judge()
    }

    fn resolve_ref<'a>(&'a self, target: &'a ModelRef) -> Option<(&'a ProviderConfig, &'a str)> {
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
