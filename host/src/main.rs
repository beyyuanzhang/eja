mod config;
mod hands;
mod http;
mod llm;
mod mcp;
mod oauth;
mod runtime;
mod sandbox;
mod state;
mod tool;

use runtime::{EventSink, HostCommand, HostRuntime, OpenError};
use serde_json::{json, Value};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

fn read_commands(reader: impl BufRead, sender: Sender<Result<HostCommand, String>>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let command = serde_json::from_str(&line).map_err(|err| format!("无效指令: {err}"));
        if sender.send(command).is_err() {
            return;
        }
    }
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

fn send_owner(value: &Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() {
    let emit: EventSink = Arc::new(|value| send_owner(&value));

    let mut runtime = match HostRuntime::open(parse_data_dir(), emit) {
        Ok(rt) => rt,
        Err(OpenError::AlreadyRunning) => std::process::exit(0),
        Err(OpenError::Failed(err)) => {
            send_owner(&json!({ "event": "error", "message": format!("初始化失败: {err}") }));
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || read_commands(io::stdin().lock(), tx));
    if let Err(err) = runtime.run(rx) {
        runtime.emit_error(err);
    }
}
