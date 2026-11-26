use crate::sandbox::TrustedDir;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
#[cfg(test)]
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_api_key: bool,
    #[serde(default)]
    pub models: Vec<String>,
}

impl ProviderConfig {
    pub fn is_usable(&self) -> bool {
        self.api_root().is_ok()
    }

    pub fn api_root(&self) -> Result<String, String> {
        let base = self.base_url.trim().trim_end_matches('/');
        let invalid_url = "模型服务地址需要有效的 HTTP 或 HTTPS URL";
        let (scheme, rest) = base.split_once("://").ok_or(invalid_url)?;
        let authority = rest.split('/').next().unwrap_or_default();
        if !["http", "https"]
            .iter()
            .any(|http| scheme.eq_ignore_ascii_case(http))
            || authority.is_empty()
            || authority.contains('@')
            || base.contains(['?', '#', '\\'])
            || base.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(invalid_url.into());
        }
        let path = &rest[authority.len()..];
        let origin = format!("{}://{authority}", scheme.to_ascii_lowercase());
        match self.kind {
            ProviderKind::OpenAi => Ok(format!(
                "{origin}{}",
                path.strip_suffix("/chat/completions").unwrap_or(path)
            )),
            ProviderKind::Anthropic => match path.strip_suffix("/messages") {
                Some(root) => Ok(format!("{origin}{root}")),
                None if path.ends_with("/v1") => Ok(format!("{origin}{path}")),
                None => Ok(format!("{origin}{path}/v1")),
            },
        }
    }

    pub fn reuse_api_key(&mut self, providers: &[Self]) {
        self.has_api_key = false;
        if self.api_key.trim().is_empty() {
            self.api_key.clear();
            if let Ok(root) = self.api_root() {
                if let Some(existing) = providers.iter().find(|p| {
                    p.id == self.id
                        && p.kind == self.kind
                        && p.api_root().is_ok_and(|existing| existing == root)
                }) {
                    self.api_key.clone_from(&existing.api_key);
                }
            }
        }
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
pub enum CapabilityTransport {
    Builtin,
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<Value>,
    },
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

impl CapabilityTransport {
    pub fn is_mcp(&self) -> bool {
        matches!(self, Self::Stdio { .. } | Self::Http { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub transport: CapabilityTransport,
}

impl CapabilityConfig {
    pub fn builtin_shell() -> Self {
        Self {
            id: "shell".into(),
            name: "Shell".into(),
            enabled: true,
            transport: CapabilityTransport::Builtin,
        }
    }
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
    pub capabilities: Vec<CapabilityConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_max_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_learning: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    #[serde(default)]
    pub providers: Option<Vec<ProviderConfig>>,
    #[serde(default)]
    pub judge: Option<ModelRef>,
    #[serde(default)]
    pub capabilities: Option<Vec<CapabilityConfig>>,
    #[serde(default)]
    pub window_max_bytes: Option<usize>,
    #[serde(default)]
    pub max_steps: Option<usize>,
    #[serde(default)]
    pub recall_learning: Option<bool>,
}

impl From<HostConfig> for ConfigPatch {
    fn from(config: HostConfig) -> Self {
        Self {
            providers: Some(config.providers),
            judge: Some(config.judge),
            capabilities: Some(config.capabilities),
            window_max_bytes: config.window_max_bytes,
            max_steps: config.max_steps,
            recall_learning: config.recall_learning,
        }
    }
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
                has_api_key: false,
                models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            }],
            judge: ModelRef {
                provider_id: "openai".into(),
                model: "gpt-4o".into(),
            },
            capabilities: vec![CapabilityConfig::builtin_shell()],
            window_max_bytes: None,
            max_steps: None,
            recall_learning: None,
        }
    }
}

impl HostConfig {
    #[cfg(test)]
    pub fn config_path(core_dir: &Path) -> PathBuf {
        core_dir.join("config.json")
    }

    #[cfg(test)]
    pub fn capabilities_path(data_dir: &Path) -> PathBuf {
        data_dir.join("capabilities.json")
    }

    pub fn load(core_dir: &TrustedDir) -> Self {
        core_dir
            .read_file("config.json")
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn normalize_capabilities(
        caps: Vec<CapabilityConfig>,
    ) -> Result<Vec<CapabilityConfig>, String> {
        let mut shell = CapabilityConfig::builtin_shell();
        let mut ids = HashSet::new();
        let mut others = Vec::with_capacity(caps.len() + 1);
        for cap in caps {
            if cap.id.trim().is_empty() || !ids.insert(cap.id.clone()) {
                return Err(format!("能力 ID 为空或重复：{}", cap.id));
            }
            match (&cap.transport, cap.id.as_str()) {
                (CapabilityTransport::Builtin, "shell") => {
                    if !cap.name.trim().is_empty() {
                        shell.name = cap.name;
                    }
                }
                (CapabilityTransport::Builtin, _) | (_, "shell") => {
                    return Err("内置 Shell 使用保留 ID shell".into());
                }
                _ => others.push(cap),
            }
        }
        others.insert(0, shell);
        Ok(others)
    }

    pub fn read_capabilities(data_dir: &TrustedDir) -> Result<Option<Vec<u8>>, String> {
        match data_dir.read_file("capabilities.json") {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("读取 capabilities.json 失败: {err}")),
        }
    }

    pub fn parse_capabilities(bytes: Option<&[u8]>) -> Result<Vec<CapabilityConfig>, String> {
        let parsed: Vec<CapabilityConfig> = serde_json::from_slice(bytes.unwrap_or(b"[]"))
            .map_err(|err| format!("解析 capabilities.json 失败: {err}"))?;
        Self::normalize_capabilities(parsed)
    }

    pub fn save_models(&self, core_dir: &TrustedDir) -> Result<(), String> {
        let mut core_value = serde_json::to_value(self).map_err(|err| err.to_string())?;
        if let Some(map) = core_value.as_object_mut() {
            map.remove("capabilities");
        }
        write_json_atomic(core_dir, "config.json", &core_value)
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

    pub fn recall_learning(&self) -> bool {
        self.recall_learning.unwrap_or(false)
    }

    pub fn redacted_for_ui(&self) -> Self {
        let mut copy = self.clone();
        for provider in &mut copy.providers {
            provider.has_api_key = !provider.api_key.is_empty();
            provider.api_key.clear();
        }
        copy
    }

    pub fn preserve_api_keys(&mut self, current: &Self) {
        for provider in &mut self.providers {
            provider.reuse_api_key(&current.providers);
        }
    }
}

pub(crate) fn write_json_atomic<T: Serialize>(
    dir: &TrustedDir,
    name: &str,
    value: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    dir.write_file_atomic(name, &bytes)
        .map_err(|err| err.to_string())
}
