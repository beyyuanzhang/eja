mod config;
mod llm;
mod mcp;
mod runtime;

use config::{HostConfig, ProviderConfig};
use runtime::HostRuntime;
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Send { text: String },
    SaveConfig { config: HostConfig },
    TestProvider {
        id: String,
        provider: ProviderConfig,
        model: String,
    },
    ReloadConnectors,
    Consolidate,
}

fn default_data_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Eja")
}

fn parse_data_dir() -> PathBuf {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data-dir" {
            if let Some(val) = args.next() {
                return PathBuf::from(val);
            }
        }
    }
    default_data_dir()
}

fn main() {
    let data_dir = parse_data_dir();
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let emit = {
        let out = Arc::clone(&stdout);
        Arc::new(move |val: Value| {
            if let Ok(line) = serde_json::to_string(&val) {
                let mut guard = out.lock().unwrap();
                let _ = writeln!(guard, "{line}");
                let _ = guard.flush();
            }
        })
    };

    let runtime = match HostRuntime::open(data_dir, emit.clone()) {
        Ok(rt) => Arc::new(rt),
        Err(err) => {
            emit(serde_json::json!({ "event": "error", "message": format!("初始化失败: {err}") }));
            std::process::exit(1);
        }
    };
    runtime.emit_state();

    let wake_rt = Arc::clone(&runtime);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(2));
        wake_rt.check_due_wakes();
    });

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(raw) = line else { break };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = match serde_json::from_str::<Command>(trimmed) {
            Ok(cmd) => cmd,
            Err(err) => {
                runtime.emit_error(format!("无效指令: {err}"));
                continue;
            }
        };
        let rt = Arc::clone(&runtime);
        match parsed {
            Command::Send { text } => {
                thread::spawn(move || rt.handle_user_message(&text));
            }
            Command::SaveConfig { config } => {
                thread::spawn(move || rt.save_config(config));
            }
            Command::TestProvider {
                id,
                provider,
                model,
            } => {
                thread::spawn(move || rt.test_provider(id, provider, model));
            }
            Command::ReloadConnectors => {
                thread::spawn(move || rt.reload_connectors());
            }
            Command::Consolidate => {
                thread::spawn(move || rt.consolidate_now(true));
            }
        }
    }
}
