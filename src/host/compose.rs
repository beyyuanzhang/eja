use super::{
    capability::{CHECKPOINTS, FILES, GIT_WORKTREES, MANAGED_PLUGINS, MCP, SHELL},
    connectors::connector_config,
    plugins,
    settings::{Effective, RoleConfig},
    SettingsStore,
};
use eja::adapters::{
    Checkpoints, Control, EnvironmentProvider, GitWorktrees, McpTools, NativeWorker, OpenAiBrain,
    OpenAiModel, Ports, RecordFiles, SharedEnvironment, ToolProvider, ToolRegistry,
    WorkspaceEnvironments, WorkspaceTools,
};
use eja::{JournalLease, Store, TurnOptions, TurnOutcome, TurnRun};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The host's one composition result. Tools, environments and snapshots are
/// services consumed by the worker or exposed to the kernel through Ports.
pub struct ComposedHost {
    pub effective: Effective,
    /// One grant snapshot for this composition. Tool construction, the default
    /// worker workspace and the turn's OPTIONS all derive from this vector.
    pub grants: Vec<eja::WorkspaceGrant>,
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

    /// The one turn assembly used by CLI, HTTP and the host's pulse. Limits and
    /// grants come from this composition, not from a second settings read.
    pub fn run_turn<'i>(
        &self,
        state_dir: &Path,
        store: &Store,
        input: &'i str,
        base: &TurnOptions,
        lease: Option<JournalLease>,
    ) -> eja::Result<TurnOutcome<'i>> {
        let eja_role = self
            .effective
            .roles
            .eja
            .as_ref()
            .filter(|role| role.configured())
            .ok_or("settings need an EJA provider and model")?;
        let aja_role = self
            .effective
            .roles
            .aja
            .as_ref()
            .filter(|role| role.configured())
            .ok_or("settings need an AJA provider and model")?;
        let mut brain = OpenAiBrain::new(&eja_role.base_url, &eja_role.api_key, &eja_role.model)
            .with_headers(header_pairs(eja_role));
        if let Some(description) = &self.capability_description {
            brain = brain.with_capability_description(description);
        }
        let model = OpenAiModel::new(&aja_role.base_url, &aja_role.api_key, &aja_role.model)
            .with_headers(header_pairs(aja_role));
        let mut aja_limits = eja::aja::Limits::default();
        if self.effective.limits.aja_steps > 0 {
            aja_limits.steps = Some(self.effective.limits.aja_steps);
        }
        if self.effective.limits.aja_timeout_ms > 0 {
            aja_limits.timeout_ms = self.effective.limits.aja_timeout_ms;
        }
        let ajas = state_dir.join("ajas");
        let worker = self.worker(&ajas, model, aja_limits)?;
        let records = RecordFiles::with_checkpoints(&ajas, state_dir.join("checkpoints"));
        let ports = Ports {
            aja: Some(&records),
            capabilities: Some(&*self.tools),
            environments: self.environments.as_deref(),
        };
        let mut options = base.clone();
        options.workspaces.clone_from(&self.grants);
        options.limits.slots = self.effective.limits.slots.max(1);
        if self.effective.limits.turn_timeout_ms > 0 {
            options.limits.timeout_ms = self.effective.limits.turn_timeout_ms;
        }
        let run = TurnRun {
            store,
            input,
            brain: &brain,
            worker: &worker,
            host: &ports,
            now: eja::now_utc(),
            options: &options,
        };
        if let Some(lease) = lease {
            run.execute_under_lease(lease)
        } else {
            run.execute()
        }
    }
}

/// Read one deployment's settings and construct the capabilities shared by CLI
/// and HTTP turns. Optional providers report failures in `diagnostics`.
pub fn compose(settings: &SettingsStore, startup: &Control) -> eja::Result<ComposedHost> {
    let mut diagnostics = Vec::new();
    let effective = settings.effective_current()?;
    let capabilities = &effective.settings.capabilities;

    let grants = effective.grants(settings);

    let private = settings.state_dir().canonicalize()?;
    // The project directory is startup configuration, not an independent
    // authority. A root reaches tools and workers only through this turn's
    // capability-filtered grant snapshot.
    let workspace = grants.first().map(|grant| PathBuf::from(&grant.root));
    let roots = grants
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
        &grants,
        effective.limits.slots,
        capabilities.enabled(GIT_WORKTREES),
        startup,
    );
    diagnostics.extend(environment_diagnostics);
    let snapshots = (capabilities.enabled(CHECKPOINTS) && !grants.is_empty())
        .then(|| Checkpoints::new(private.join("checkpoints"), private.clone()));

    Ok(ComposedHost {
        effective,
        grants,
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
        configure_with_workspaces(settings, capabilities, &json!({}));
    }

    fn configure_with_workspaces(
        settings: &SettingsStore,
        capabilities: &serde_json::Value,
        workspaces: &serde_json::Value,
    ) {
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
                "workspaces": workspaces
            }))
            .expect("settings");
    }

    #[test]
    fn a_disabled_local_workspace_does_not_fall_back_to_the_project_directory() {
        let root = Temp::new("no-local-workspace");
        let project = root.0.join("project");
        let state = root.0.join("state");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(project.join("note.txt"), "must not be read").unwrap();

        let settings = SettingsStore::new(Some(project), state);
        configure(
            &settings,
            &json!({
                "local-workspace": {"enabled": false},
                "files": {"enabled": true},
                "shell": {"enabled": true},
                "git-worktrees": {"enabled": true},
                "checkpoints": {"enabled": true},
                "mcp": {"enabled": false, "config": {"servers": []}},
                "managed-plugins": {"enabled": false}
            }),
        );
        let control = Control::new(Duration::from_secs(30), Arc::default());
        let composed = compose(&settings, &control).expect("composition");

        assert!(composed.grants.is_empty());
        assert!(composed.workspace.is_none());
        assert!(composed.environments.is_none());
        assert!(composed.snapshots.is_none());
        assert!(!definition_names(&composed.tools)
            .iter()
            .any(|name| name == "read_file"));
    }

    #[test]
    fn an_extra_workspace_is_the_one_source_for_tools_and_the_default() {
        let root = Temp::new("extra-workspace");
        let extra = root.0.join("extra");
        let state = root.0.join("state");
        fs::create_dir_all(&extra).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(extra.join("note.txt"), "only the extra grant").unwrap();

        let settings = SettingsStore::new(None, state);
        configure_with_workspaces(
            &settings,
            &json!({
                "local-workspace": {"enabled": true},
                "files": {"enabled": true},
                "shell": {"enabled": false},
                "git-worktrees": {"enabled": false},
                "checkpoints": {"enabled": true},
                "mcp": {"enabled": false, "config": {"servers": []}},
                "managed-plugins": {"enabled": false}
            }),
            &json!({"extra": extra}),
        );
        let control = Control::new(Duration::from_secs(30), Arc::default());
        let composed = compose(&settings, &control).expect("composition");

        assert_eq!(composed.grants.len(), 1);
        assert_eq!(composed.grants[0].label, "extra");
        assert_eq!(
            composed.workspace.as_deref(),
            Some(extra.canonicalize().unwrap().as_path())
        );
        let observed = composed
            .tools
            .observe("read_file", &json!({"path":"note.txt"}), &control)
            .expect("the extra grant is readable");
        assert!(observed.to_string().contains("only the extra grant"));
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
