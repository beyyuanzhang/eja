mod config;
mod hands;
mod http;
mod llm;
mod mcp;
mod oauth;
mod runtime;
mod sandbox;
mod state;

use config::{ConfigPatch, ProviderConfig};
use runtime::{EventSink, HostRuntime};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Send {
        text: String,
    },
    SaveConfig {
        config: ConfigPatch,
    },
    TestProvider {
        id: String,
        provider: ProviderConfig,
        model: String,
    },
    OauthConnector {
        id: String,
    },
    ReloadConnectors,
}

enum Input {
    Line(String),
    Closed,
}

fn read_commands(reader: impl BufRead, sender: Sender<Input>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !line.trim().is_empty() && sender.send(Input::Line(line)).is_err() {
            return;
        }
    }
    let _ = sender.send(Input::Closed);
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
    let emit: EventSink = Box::new(|value| send_owner(&value));

    let mut runtime = match HostRuntime::open(parse_data_dir(), emit) {
        Ok(rt) => rt,
        Err(err) => {
            send_owner(&json!({ "event": "error", "message": format!("初始化失败: {err}") }));
            std::process::exit(1);
        }
    };
    runtime.emit_state();
    runtime.process_pending_turn();

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || read_commands(io::stdin().lock(), tx));

    loop {
        let next = match runtime.next_timer_delay() {
            Ok(Some(delay)) if delay.is_zero() => {
                if let Err(err) = runtime.tick_timers() {
                    runtime.emit_error(format!("处理定时任务失败: {err}"));
                    break;
                }
                continue;
            }
            Ok(Some(delay)) => match rx.recv_timeout(delay) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(err) = runtime.tick_timers() {
                        runtime.emit_error(format!("处理定时任务失败: {err}"));
                        break;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            Ok(None) => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            },
            Err(err) => {
                runtime.emit_error(format!("读取唤醒计划失败: {err}"));
                break;
            }
        };
        let line = match next {
            Input::Closed => break,
            Input::Line(line) => line,
        };
        match serde_json::from_str::<Command>(&line) {
            Ok(Command::Send { text }) => runtime.handle_user_message(&text),
            Ok(Command::SaveConfig { config }) => runtime.save_config(config),
            Ok(Command::TestProvider {
                id,
                provider,
                model,
            }) => runtime.test_provider(id, provider, model),
            Ok(Command::OauthConnector { id }) => runtime.oauth_connector(id),
            Ok(Command::ReloadConnectors) => runtime.reload_connectors(),
            Err(err) => runtime.emit_error(format!("无效指令: {err}")),
        }
    }
}
