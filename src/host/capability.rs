//! One flat catalog for built-in and installed capabilities.
//!
//! The catalog describes services. It does not add an authority: settings are
//! user choices, while each provider still owns its protocol, grants and
//! lifecycle. Adding a plugin capability therefore does not add a kernel type.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const LOCAL_WORKSPACE: &str = "local-workspace";
pub const FILES: &str = "files";
pub const SHELL: &str = "shell";
pub const GIT_WORKTREES: &str = "git-worktrees";
pub const CHECKPOINTS: &str = "checkpoints";
pub const MCP: &str = "mcp";
pub const MANAGED_PLUGINS: &str = "managed-plugins";

const BUILTINS: [(&str, &str, &str, &str, bool); 7] = [
    (
        LOCAL_WORKSPACE,
        "本地工作区",
        "把用户授权的工作目录提供给执行环境。",
        "workspace",
        true,
    ),
    (
        FILES,
        "文件工具",
        "读取、写入、列出、搜索和编辑工作区文件。",
        "tools",
        true,
    ),
    (
        SHELL,
        "Shell 工具",
        "在本次执行的环境中运行受预算约束的命令。",
        "tools",
        true,
    ),
    (
        GIT_WORKTREES,
        "Git 隔离环境",
        "为并行执行建立独立 worktree，并保留其分支结果。",
        "environment",
        true,
    ),
    (
        CHECKPOINTS,
        "执行证据快照",
        "把一次执行可见的工作区文件冻结为证据提交。",
        "evidence",
        true,
    ),
    (
        MCP,
        "MCP 工具",
        "通过 MCP 连接器提供与内置工具同级的动作能力。",
        "tools",
        true,
    ),
    (
        MANAGED_PLUGINS,
        "插件编写工具",
        "允许 EJA 编写、测试、激活和回滚隔离的 MCP 插件。",
        "plugins",
        false,
    ),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilitySetting {
    pub enabled: bool,
    #[serde(default = "empty_object")]
    pub config: Value,
}

impl CapabilitySetting {
    pub const fn new(enabled: bool, config: Value) -> Self {
        Self { enabled, config }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct CapabilitySettings {
    entries: BTreeMap<String, CapabilitySetting>,
}

impl CapabilitySettings {
    pub const fn from_entries(entries: BTreeMap<String, CapabilitySetting>) -> Self {
        Self { entries }
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|entry| entry.enabled)
    }

    pub fn config(&self, id: &str) -> Value {
        self.entries
            .get(id)
            .map_or_else(empty_object, |entry| entry.config.clone())
    }

    pub const fn entries(&self) -> &BTreeMap<String, CapabilitySetting> {
        &self.entries
    }

    pub fn normalize(mut self, descriptors: &[CapabilityDescriptor]) -> Self {
        for descriptor in descriptors {
            self.ensure(descriptor);
        }
        self
    }

    pub fn ensure(&mut self, descriptor: &CapabilityDescriptor) {
        self.entries
            .entry(descriptor.id.clone())
            .or_insert_with(|| {
                CapabilitySetting::new(
                    descriptor.default_enabled,
                    descriptor.default_config.clone(),
                )
            });
    }
}

impl Default for CapabilitySettings {
    fn default() -> Self {
        let mut entries = BTreeMap::new();
        for descriptor in builtins() {
            entries.insert(
                descriptor.id,
                CapabilitySetting::new(descriptor.default_enabled, descriptor.default_config),
            );
        }
        Self { entries }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub source: String,
    pub default_enabled: bool,
    #[serde(skip_serializing)]
    pub default_config: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityView {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub source: String,
    pub enabled: bool,
    pub config: Value,
}

pub fn builtins() -> Vec<CapabilityDescriptor> {
    BUILTINS
        .into_iter()
        .map(
            |(id, title, description, kind, default_enabled)| CapabilityDescriptor {
                id: id.to_owned(),
                title: title.to_owned(),
                description: description.to_owned(),
                kind: kind.to_owned(),
                source: "builtin".to_owned(),
                default_enabled,
                default_config: default_config(id),
            },
        )
        .collect()
}

pub fn views(
    descriptors: &[CapabilityDescriptor],
    settings: &CapabilitySettings,
) -> Vec<CapabilityView> {
    descriptors
        .iter()
        .map(|descriptor| CapabilityView {
            id: descriptor.id.clone(),
            title: descriptor.title.clone(),
            description: descriptor.description.clone(),
            kind: descriptor.kind.clone(),
            source: descriptor.source.clone(),
            enabled: settings.enabled(&descriptor.id),
            config: settings.config(&descriptor.id),
        })
        .collect()
}

fn default_config(id: &str) -> Value {
    if id == MCP {
        json!({"servers":[]})
    } else {
        empty_object()
    }
}

pub fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_cards_are_filled_without_erasing_installed_cards() {
        let descriptors = builtins();
        let mut settings = CapabilitySettings {
            entries: BTreeMap::new(),
        };
        settings.entries.insert(
            "plugin:demo".to_owned(),
            CapabilitySetting::new(false, json!({"answer":42})),
        );

        let normalized = settings.normalize(&descriptors);

        assert!(normalized.enabled(LOCAL_WORKSPACE));
        assert!(!normalized.enabled(MANAGED_PLUGINS));
        assert!(!normalized.enabled("plugin:demo"));
        assert_eq!(normalized.config("plugin:demo")["answer"], 42);
    }
}
