//! Host-owned versions of self-authored, isolated MCP servers.

use super::{
    plugin_sandbox,
    util::{write_private_bytes, write_private_json},
};
use eja::{
    adapters::{HostTools, McpTools, ToolContext, ToolGateway},
    Assignment, Control, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder},
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SOURCE_BYTES: usize = 64 * 1024;
const MAX_ACTIVE: usize = 16;
static VERSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const TOOLS: [(&str, &str, &[&str]); 5] = [
    ("eja_plugin_list", "List managed plugins, their active versions, and startup errors.", &[]),
    ("eja_plugin_stage", "Stage a complete Python 3 stdio MCP server as a new version. Use only the standard library. The process cannot access the network, services, or project files. Protocol lines go to stdout. Test a tool before activation.", &["name", "source"]),
    ("eja_plugin_test", "Start a staged version in isolation and call the named MCP tool with sample arguments. Only a successful test makes the version eligible for activation.", &["name", "version", "tool", "arguments"]),
    ("eja_plugin_activate", "Activate a tested version for the next turn. Tools are named plugin__NAME__TOOL. This does not change the tools in the current turn.", &["name", "version"]),
    ("eja_plugin_rollback", "Restore the previous version for the next turn. If there is no previous version, deactivate the plugin.", &["name"]),
];

#[derive(Debug, Default, Serialize, Deserialize)]
struct Activation {
    current: Option<String>,
    previous: Option<String>,
}

type Activations = BTreeMap<String, Activation>;

struct Manager {
    root: PathBuf,
    writes: Mutex<()>,
    loaded: BTreeMap<String, Value>,
}

impl Manager {
    fn read(&self) -> Result<Activations> {
        let index = eja::adapters::workspace_path(&self.root, "active.json")?;
        match fs::read(index) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn version_path(&self, name: &str, version: &str) -> Result<PathBuf> {
        if !valid_name(name)
            || version.is_empty()
            || version.len() > 64
            || !version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
        {
            return Err("invalid managed plugin name or version".into());
        }
        eja::adapters::workspace_path(&self.root, &format!("{name}/{version}"))
    }

    fn stage(&self, name: &str, source: &str) -> Result<Value> {
        if source.trim().is_empty() || source.len() > SOURCE_BYTES {
            return Err("plugin source must contain between 1 byte and 64 KiB".into());
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let sequence = VERSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let version = format!("{now}-{}-{sequence}", std::process::id());
        let dir = self.version_path(name, &version)?;
        let parent = dir.parent().ok_or("missing plugin directory")?;
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(parent)?;
        DirBuilder::new().mode(0o700).create(&dir)?;
        write_private_bytes(&dir.join("plugin.py"), source.as_bytes())
            .map_err(|error| error.to_string())?;
        Ok(json!({"name": name, "version": version, "status": "staged"}))
    }

    fn load_version(&self, name: &str, version: &str, control: &Control) -> Result<McpTools> {
        let dir = self.version_path(name, version)?;
        let source = eja::adapters::workspace_path(&dir, "plugin.py")?;
        if !source.is_file() {
            return Err("managed plugin version has no source".into());
        }
        let (launcher, args) = plugin_sandbox::command(&source)?;
        McpTools::load_managed(name, launcher, args, &dir, control)
    }

    fn test(
        &self,
        name: &str,
        version: &str,
        args: &Value,
        context: &ToolContext<'_>,
    ) -> Result<Value> {
        let tool = required(args, "tool")?;
        let arguments = args
            .get("arguments")
            .filter(|value| value.is_object())
            .ok_or("arguments must be an object")?;
        let control = context.control.child(Duration::from_secs(10));
        let gateway = self.load_version(name, version, &control)?;
        let public = McpTools::managed_tool_name(name, tool);
        let result = gateway.call(
            &ToolContext {
                workspace: None,
                control: &control,
                ..*context
            },
            &public,
            arguments,
        )?;
        if result["isError"] == true {
            return Err("the plugin's sample tool call failed".into());
        }
        control.check()?;
        write_private_json(
            &self.version_path(name, version)?.join("tested.json"),
            &json!({"tool":public}),
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({"name":name,"version":version,"tool":public,"status":"tested","result":result}))
    }

    fn select(&self, name: &str, version: Option<&str>, control: &Control) -> Result<Value> {
        let _guard = self
            .writes
            .lock()
            .map_err(|_| "managed plugin lock is unavailable")?;
        control.check()?;
        let mut entries = self.read()?;
        if let Some(version) = version {
            let dir = self.version_path(name, version)?;
            if !eja::adapters::workspace_path(&dir, "tested.json")?.is_file() {
                return Err("test this version successfully before activating it".into());
            }
            let entry = entries.entry(name.to_owned()).or_default();
            if entry.current.as_deref() != Some(version) {
                entry.previous = entry.current.replace(version.into());
            }
        } else {
            let entry = entries
                .get_mut(name)
                .ok_or("this plugin has no activation to roll back")?;
            std::mem::swap(&mut entry.current, &mut entry.previous);
        }
        if entries
            .values()
            .filter(|entry| entry.current.is_some())
            .count()
            > MAX_ACTIVE
        {
            return Err("managed plugin roster is full".into());
        }
        let index = eja::adapters::workspace_path(&self.root, "active.json")?;
        write_private_json(&index, &serde_json::to_value(&entries)?)
            .map_err(|error| error.to_string())?;
        Ok(json!({"name":name,"applies":"next turn","activation":entries[name]}))
    }
}

impl ToolGateway for Manager {
    fn definitions(&self, _: &Assignment) -> Vec<Value> {
        TOOLS.iter().map(|(name, description, required)| {
            let properties: serde_json::Map<String, Value> = required.iter().map(|field| {
                ((*field).into(), json!({"type": if *field == "arguments" {"object"} else {"string"}}))
            }).collect();
            json!({"type":"function","function":{
                "name":name,"description":description,
                "parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}
            }})
        }).collect()
    }

    fn owns(&self, name: &str) -> bool {
        TOOLS.iter().any(|(tool, _, _)| *tool == name)
    }

    fn call(&self, context: &ToolContext<'_>, tool: &str, arguments: &Value) -> Result<Value> {
        context.control.check()?;
        if tool == "eja_plugin_list" {
            return Ok(json!({"activation":self.read()?,"loaded_for_this_turn":self.loaded}));
        }
        let name = required(arguments, "name")?;
        if !valid_name(name) {
            return Err(
                "plugin names use 1 to 24 lowercase ASCII letters, digits or hyphens".into(),
            );
        }
        match tool {
            "eja_plugin_stage" => self.stage(name, required(arguments, "source")?),
            "eja_plugin_test" => {
                self.test(name, required(arguments, "version")?, arguments, context)
            }
            "eja_plugin_activate" => {
                self.select(name, Some(required(arguments, "version")?), context.control)
            }
            "eja_plugin_rollback" => self.select(name, None, context.control),
            _ => Err("unknown managed plugin operation".into()),
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 24
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn required<'a>(arguments: &'a Value, field: &str) -> Result<&'a str> {
    arguments[field]
        .as_str()
        .ok_or_else(|| format!("{field} must be a string").into())
}

struct LoadedPlugin {
    name: String,
    version: String,
    gateway: McpTools,
}

impl ToolGateway for LoadedPlugin {
    fn definitions(&self, assignment: &Assignment) -> Vec<Value> {
        self.gateway.definitions(assignment)
    }

    fn owns(&self, name: &str) -> bool {
        self.gateway.owns(name)
    }

    fn call(&self, context: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<Value> {
        let result = self.gateway.call(context, name, arguments)?;
        Ok(json!({"plugin":self.name,"version":self.version,"result":result}))
    }
}

pub(super) fn register(tools: &mut HostTools, state: &Path, parent: &Control) -> Result<()> {
    let root = eja::adapters::workspace_path(&state.canonicalize()?, "plugins")?;
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&root)?;
    let mut manager = Manager {
        root,
        writes: Mutex::new(()),
        loaded: BTreeMap::new(),
    };
    let startup = parent.child(Duration::from_secs(20));
    for (name, entry) in manager.read()? {
        let Some(current) = entry.current else {
            continue;
        };
        let mut status = json!({"requested":current});
        let mut loaded = manager
            .load_version(&name, &current, &startup)
            .map(|gateway| (current.clone(), gateway));
        if let Err(error) = &loaded {
            status["error"] = json!(error.to_string());
            if let Some(previous) = entry.previous {
                loaded = manager
                    .load_version(&name, &previous, &startup)
                    .map(|gateway| (previous, gateway));
            }
        }
        if let Ok((version, gateway)) = loaded {
            status["version"] = json!(version);
            tools.register(Box::new(LoadedPlugin {
                name: name.clone(),
                version,
                gateway,
            }));
        }
        manager.loaded.insert(name, status);
    }
    tools.register(Box::new(manager));
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{net::TcpListener, sync::Arc};

    const SOURCE: &str = include_str!("../../tests/fixtures/managed_plugin.py");

    struct Remove(PathBuf);

    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn managed_plugins_are_isolated_versioned_and_recoverable() {
        let path = std::env::temp_dir().join(format!("eja-plugins-test-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let root = Remove(path.canonicalize().unwrap());
        let state = &root.0;
        let secret = state.join("private.txt");
        fs::write(&secret, "never given to a plugin").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let control = Control::new(Duration::from_secs(60), Arc::default());
        let context = ToolContext {
            aja_id: 1,
            workspace: None,
            private: state,
            control: &control,
        };
        let fresh = || {
            let mut tools = HostTools::with_workspace(None, None, state, &control).unwrap();
            register(&mut tools, state, &control).unwrap();
            tools
        };
        let name = "sample";
        let public = "plugin__sample__format";
        let initial = fresh();
        let stage_version = |tools: &HostTools, source: &str| {
            let reply = tools
                .call(
                    &context,
                    "eja_plugin_stage",
                    &json!({"name":name,"source":source}),
                )
                .unwrap();
            reply["version"].as_str().unwrap().to_owned()
        };
        let sample = |tools: &HostTools, version: &str, fail: bool| {
            tools.call(&context, "eja_plugin_test", &json!({
                "name":name,"version":version,"tool":"format","arguments":{"text":"hello","private":secret,"port":port,"fail":fail}
            }))
        };
        let activate = |tools: &HostTools, version: &str| {
            tools.call(
                &context,
                "eja_plugin_activate",
                &json!({"name":name,"version":version}),
            )
        };
        let first = stage_version(&initial, SOURCE);
        assert!(activate(&initial, &first).is_err());
        assert!(sample(&initial, &first, true).is_err());
        assert!(activate(&initial, &first).is_err());
        let output = sample(&initial, &first, false).unwrap();
        let output = &output["result"]["structuredContent"];
        assert_eq!(output["value"], "one:hello");
        for denied in [
            "environment_leaked",
            "private_readable",
            "private_writable",
            "network_allowed",
        ] {
            assert_eq!(output[denied], false, "{output}");
        }
        activate(&initial, &first).unwrap();
        assert!(!initial.owns(public));
        let first_turn = fresh();
        let second = stage_version(
            &first_turn,
            &SOURCE.replace("REVISION = \"one\"", "REVISION = \"two\""),
        );
        sample(&first_turn, &second, false).unwrap();
        activate(&first_turn, &second).unwrap();
        let use_plugin = |tools: &HostTools| {
            tools
                .call(&context, public, &json!({"text":"later"}))
                .unwrap()
        };
        assert_eq!(use_plugin(&first_turn)["version"], first);
        let second_turn = fresh();
        let used = use_plugin(&second_turn);
        assert_eq!(used["version"], second);
        assert_eq!(used["result"]["structuredContent"]["value"], "two:later");

        // The host can find the previous version even if startup of the new one fails.
        fs::remove_file(
            state
                .join("plugins")
                .join(name)
                .join(&second)
                .join("plugin.py"),
        )
        .unwrap();
        let failed_start = fresh();
        assert_eq!(use_plugin(&failed_start)["version"], first);
        let listed = failed_start
            .call(&context, "eja_plugin_list", &json!({}))
            .unwrap();
        assert!(listed["loaded_for_this_turn"][name]["error"].is_string());
        failed_start
            .call(&context, "eja_plugin_rollback", &json!({"name":name}))
            .unwrap();
        assert_eq!(use_plugin(&fresh())["version"], first);
        drop(listener);
    }
}
