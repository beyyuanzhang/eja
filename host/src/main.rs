mod config;
mod hands;
mod llm;
mod mcp;
mod runtime;
mod sandbox;

use config::{HostConfig, ProviderConfig};
use runtime::{EventSink, HostRuntime};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Send {
        text: String,
    },
    SaveConfig {
        config: HostConfig,
    },
    TestProvider {
        id: String,
        provider: ProviderConfig,
        model: String,
    },
    ReloadConnectors,
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
    let emit: EventSink = Box::new(|val: Value| {
        if let Ok(line) = serde_json::to_string(&val) {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    });

    let mut runtime = match HostRuntime::open(data_dir, emit) {
        Ok(rt) => rt,
        Err(err) => {
            let payload =
                serde_json::json!({ "event": "error", "message": format!("初始化失败: {err}") });
            if let Ok(line) = serde_json::to_string(&payload) {
                println!("{line}");
            }
            std::process::exit(1);
        }
    };
    runtime.emit_state();

    let (tx, rx) = mpsc::channel::<Result<Command, String>>();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(raw) = line else { break };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed =
                serde_json::from_str::<Command>(trimmed).map_err(|err| format!("无效指令: {err}"));
            if tx.send(parsed).is_err() {
                break;
            }
        }
    });

    loop {
        let next = match runtime.next_wake_delay() {
            Some(delay) => match rx.recv_timeout(delay) {
                Ok(msg) => Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    runtime.check_due_wakes();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => break,
            },
        };
        match next {
            Some(Ok(Command::Send { text })) => runtime.handle_user_message(&text),
            Some(Ok(Command::SaveConfig { config })) => runtime.save_config(config),
            Some(Ok(Command::TestProvider {
                id,
                provider,
                model,
            })) => {
                runtime.test_provider(id, provider, model);
            }
            Some(Ok(Command::ReloadConnectors)) => runtime.reload_connectors(),
            Some(Err(err)) => runtime.emit_error(err),
            None => break,
        }
        runtime.check_due_wakes();
    }
}
