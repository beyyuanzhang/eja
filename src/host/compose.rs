use super::{
    capability::{CHECKPOINTS, FILES, GIT_WORKTREES, MANAGED_PLUGINS, MCP, SHELL},
    connectors::connector_config,
    plugins,
    settings::{Effective, RoleConfig},
    SettingsStore,
};
use eja::adapters::{
    Checkpoints, Control, EnvironmentProvider, GitWorktrees, McpTools, NativeWorker, OpenAiModel,
    SharedEnvironment, ToolProvider, ToolRegistry, WorkspaceEnvironments, WorkspaceTools,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The host's one composition result. Tools, environments and snapshots are
/// services consumed by the worker or exposed to the kernel through Ports.
pub struct ComposedHost {
    pub effective: Effective,
    pub tools: Arc<ToolRegistry>,
    pub environments: Option<Arc<dyn EnvironmentProvider>>,
    pub diagnostics: Vec<String>,
    pub capability_description: Option<String>,
    workspace: Option<PathBuf>,
    snapshots: Option<Checkpoints>,
}

impl ComposedHost {
    pub fn worker(
        &self,
        ajas: &Path,
        model: OpenAiModel,
        limits: eja::aja::Limits,
    ) -> eja::Result<NativeWorker> {
        NativeWorker::with_services(
            std::env::current_exe()?,
            ajas,
            Arc::new(model),
            self.tools.clone() as Arc<dyn ToolProvider>,
            eja::adapters::ExecutionServices {
                workspace: self.workspace.clone(),
                snapshots: self.snapshots.clone(),
                environments: self.environments.clone(),
            },
        )?
        .with_limits(limits)
    }
}

/// Read one deployment's settings and construct the capabilities shared by CLI
/// and HTTP turns. Optional providers report failures in `diagnostics`.
pub fn compose(settings: &SettingsStore, startup: &Control) -> eja::Result<ComposedHost> {
    let mut diagnostics = Vec::new();
    let effective = settings.effective_current()?;
    let capabilities = &effective.capabilities;

    let grant = effective.grants(settings);

    let private = settings.state_dir().canonicalize()?;
    let workspace = settings
        .project_dir()
        .map(Path::to_path_buf)
        .or_else(|| grant.first().map(|grant| PathBuf::from(&grant.root)));
    let roots = grant
        .iter()
        .map(|grant| PathBuf::from(&grant.root))
        .collect::<Vec<_>>();

    let mut providers: Vec<Box<dyn ToolProvider>> = Vec::new();
    if let Some(workspace) = &workspace {
        if capabilities.enabled(FILES) || capabilities.enabled(SHELL) {
            providers.push(Box::new(WorkspaceTools::new(
                capabilities.enabled(FILES),
                capabilities.enabled(SHELL),
                Some(workspace.clone()),
                roots,
                &private,
            )));
        }
    }
    if capabilities.enabled(MCP) && !effective.connectors.is_empty() {
        let servers = effective
            .connectors
            .iter()
            .map(|connector| (connector.name.clone(), connector_config(connector)))
            .collect();
        match McpTools::load_available_values(
            servers,
            workspace.as_deref().unwrap_or(&private),
            workspace.as_deref(),
            startup,
        ) {
            Ok((provider, errors)) => {
                diagnostics.extend(errors);
                providers.push(Box::new(provider));
            }
            Err(error) => diagnostics.push(format!("connector tools are unavailable: {error}")),
        }
    }

    let mut tools = ToolRegistry::new(&private, providers)?;
    let mut capability_description = None;
    if effective.managed_code_enabled() {
        match plugins::register(
            &mut tools,
            &private,
            startup,
            capabilities.enabled(MANAGED_PLUGINS),
            capabilities,
        ) {
            Ok(registration) => {
                diagnostics.extend(registration.diagnostics);
                capability_description = registration.description;
            }
            Err(error) => {
                diagnostics.push(format!("managed plugin tools are unavailable: {error}"));
            }
        }
    }

    let (environments, environment_diagnostics) = build_environments(
        &private,
        &grant,
        effective.limits.slots,
        capabilities.enabled(GIT_WORKTREES),
        startup,
    );
    diagnostics.extend(environment_diagnostics);
    let snapshots = (capabilities.enabled(CHECKPOINTS) && !grant.is_empty())
        .then(|| Checkpoints::new(private.join("checkpoints"), private.clone()));

    Ok(ComposedHost {
        effective,
        tools: Arc::new(tools),
        environments,
        diagnostics,
        capability_description,
        workspace,
        snapshots,
    })
}

pub(super) fn header_pairs(role: &RoleConfig) -> Vec<(String, String)> {
    role.headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// One environment provider per granted root. Parallel slots use a Git
/// worktree when available and otherwise share the root.
fn build_environments(
    state_dir: &Path,
    grant: &[eja::WorkspaceGrant],
    slots: usize,
    git: bool,
    startup: &Control,
) -> (Option<Arc<dyn EnvironmentProvider>>, Vec<String>) {
    let Some(first) = grant.first() else {
        return (None, Vec::new());
    };
    let first_root = normalize_root(&first.root);
    let mut roots: BTreeMap<PathBuf, Arc<dyn EnvironmentProvider>> = BTreeMap::new();
    let mut history: BTreeMap<PathBuf, Arc<dyn EnvironmentProvider>> = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for (index, grant) in grant.iter().enumerate() {
        let path = normalize_root(&grant.root);
        if roots.contains_key(&path) || history.contains_key(&path) {
            continue;
        }
        if git {
            match GitWorktrees::new(
                &path,
                state_dir.join("environments").join(index.to_string()),
                state_dir,
                startup,
            ) {
                Ok(worktrees) => {
                    let provider: Arc<dyn EnvironmentProvider> = Arc::new(worktrees);
                    history.insert(path.clone(), provider.clone());
                    if slots > 1 {
                        roots.insert(path.clone(), provider);
                    } else {
                        roots.insert(path.clone(), shared_environment(&path, state_dir));
                    }
                }
                Err(error) => {
                    diagnostics.push(format!(
                        "Git environment history is unavailable for {}: {error}; new runs use the shared environment and recorded worktrees cannot be reconciled",
                        path.display()
                    ));
                    roots.insert(path.clone(), shared_environment(&path, state_dir));
                }
            }
        } else {
            roots.insert(path.clone(), shared_environment(&path, state_dir));
        }
    }
    let Some(default) = roots.get(&first_root).cloned() else {
        return (None, diagnostics);
    };
    (
        Some(Arc::new(
            WorkspaceEnvironments::new(default, roots).with_history(history),
        )),
        diagnostics,
    )
}

fn shared_environment(path: &Path, state_dir: &Path) -> Arc<dyn EnvironmentProvider> {
    Arc::new(SharedEnvironment::new(
        path.to_path_buf(),
        state_dir.to_path_buf(),
    ))
}

/// Grant keys are real paths: a root reached through a symlink addresses the
/// same environment.
fn normalize_root(root: &str) -> PathBuf {
    PathBuf::from(root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eja::adapters::Capabilities as _;
    use serde_json::json;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "eja-host-{tag}-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn definition_names(tools: &ToolRegistry) -> Vec<String> {
        tools
            .definitions(&eja::Assignment {
                task_id: 0,
                objective: String::new(),
                workspace: String::new(),
                acceptance: String::new(),
                input: String::new(),
                resume: None,
                issued_at: 0,
            })
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(ToOwned::to_owned))
            .collect()
    }

    fn configure(settings: &SettingsStore, capabilities: &serde_json::Value) {
        settings
            .write(json!({
                "providers": [],
                "roles": {
                    "eja": {"providerId": "", "model": ""},
                    "aja": {"providerId": "", "model": ""}
                },
                "capabilities": capabilities,
                "limits": {
                    "slots": 1,
                    "turnTimeoutMs": 0,
                    "ajaSteps": 0,
                    "ajaTimeoutMs": 0
                },
                "workspaces": {}
            }))
            .expect("settings");
    }

    #[test]
    fn broken_optional_capabilities_are_diagnostics_not_lost_local_tools() {
        let root = Temp::new("optional");
        let project = root.0.join("project");
        let state = root.0.join("state");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(project.join("note.txt"), "local work still runs").unwrap();
        fs::write(state.join("plugins"), "not a directory").unwrap();

        let settings = SettingsStore::new(Some(project), state);
        configure(
            &settings,
            &json!({
                "local-workspace": {"enabled": true},
                "files": {"enabled": true},
                "shell": {"enabled": false},
                "git-worktrees": {"enabled": false},
                "checkpoints": {"enabled": false},
                "mcp": {"enabled": true, "config": {"servers": []}},
                "managed-plugins": {"enabled": true}
            }),
        );
        let control = Control::new(Duration::from_secs(30), Arc::default());
        let composed =
            compose(&settings, &control).expect("optional failures do not fail composition");

        let names = definition_names(&composed.tools);
        assert!(names.iter().any(|name| name == "read_file"), "{names:?}");
        assert!(
            composed
                .diagnostics
                .iter()
                .any(|error| error.contains("managed plugin tools are unavailable")),
            "{:?}",
            composed.diagnostics
        );
        let observed = composed
            .tools
            .observe("read_file", &json!({"path":"note.txt"}), &control)
            .expect("the local provider is unaffected");
        assert!(observed.to_string().contains("local work still runs"));
    }

    #[test]
    fn a_plugin_that_cannot_start_keeps_the_registry_and_reports_why() {
        let root = Temp::new("plugin-start");
        let state = root.0.join("state");
        let plugins = state.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            plugins.join("active.json"),
            serde_json::to_vec(&json!({
                "broken": {"current": "2026-01-01-1", "previous": null}
            }))
            .unwrap(),
        )
        .unwrap();

        let settings = SettingsStore::new(None, state);
        configure(
            &settings,
            &json!({
                "local-workspace": {"enabled": false},
                "files": {"enabled": false},
                "shell": {"enabled": false},
                "git-worktrees": {"enabled": false},
                "checkpoints": {"enabled": false},
                "mcp": {"enabled": false, "config": {"servers": []}},
                "managed-plugins": {"enabled": true}
            }),
        );
        let control = Control::new(Duration::from_secs(30), Arc::default());
        let composed = compose(&settings, &control).expect("plugin startup failure is optional");
        let names = definition_names(&composed.tools);
        assert!(
            names.iter().any(|name| name == "eja_plugin_list"),
            "{names:?}"
        );
        assert!(
            composed
                .diagnostics
                .iter()
                .any(|error| error.contains("managed plugin broken is unavailable")),
            "{:?}",
            composed.diagnostics
        );
    }
}
