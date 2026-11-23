mod config;
mod hands;
mod http;
mod llm;
mod mcp;
mod runtime;
mod sandbox;
mod state;
mod tools;

use config::{ConfigPatch, ProviderConfig};
use runtime::{EventSink, HostRuntime};
use sandbox::kill_process_tree;
use serde::Deserialize;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender, SyncSender};
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
    ReloadConnectors,
    LaunchUi,
    UiReady,
    OpenSettings,
}

enum Input {
    Line(Option<u32>, String),
    Closed(Option<u32>),
}

struct Panel(Child);

impl Drop for Panel {
    fn drop(&mut self) {
        kill_process_tree(&mut self.0);
    }
}

fn read_commands(reader: impl BufRead, sender: Sender<Input>, panel: Option<u32>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !line.trim().is_empty() && sender.send(Input::Line(panel, line)).is_err() {
            return;
        }
    }
    let _ = sender.send(Input::Closed(panel));
}

fn attach_panel(
    runtime: &HostRuntime,
    sender: &Sender<Input>,
) -> Result<(Panel, SyncSender<String>), String> {
    let mut panel = Panel(
        runtime
            .panel_command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| err.to_string())?,
    );
    let id = panel.0.id();
    let stdout = panel.0.stdout.take().unwrap();
    let mut stdin = panel.0.stdin.take().unwrap();
    let sender = sender.clone();
    thread::spawn(move || read_commands(BufReader::new(stdout), sender, Some(id)));
    let (output, lines) = mpsc::sync_channel::<String>(16);
    thread::spawn(move || {
        for line in lines {
            if writeln!(stdin, "{line}").is_err() {
                break;
            }
        }
    });
    Ok((panel, output))
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
    let panel_output = Rc::new(RefCell::new(None::<SyncSender<String>>));
    let output = Rc::clone(&panel_output);
    let emit: EventSink = Box::new(move |value| {
        send_owner(&value);
        let mut output = output.borrow_mut();
        if let Some(panel) = output.as_ref() {
            if panel.try_send(value.to_string()).is_err() {
                output.take();
            }
        }
    });

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
    let owner = tx.clone();
    thread::spawn(move || read_commands(io::stdin().lock(), owner, None));
    let mut panel: Option<Panel> = None;

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
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
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
        let panel_id = panel.as_ref().map(|p| p.0.id());
        let (source, line) = match next {
            Input::Closed(None) => break,
            Input::Closed(Some(id)) if Some(id) == panel_id => {
                panel_output.borrow_mut().take();
                panel.take();
                send_owner(&json!({ "event": "ui_closed" }));
                continue;
            }
            Input::Line(id, line) if id.is_none() || id == panel_id => (id, line),
            _ => continue,
        };
        match serde_json::from_str::<Command>(&line) {
            Ok(Command::Send { text }) => runtime.handle_user_message(&text),
            Ok(Command::SaveConfig { config }) if source.is_none() => runtime.save_config(config),
            Ok(Command::TestProvider {
                id,
                provider,
                model,
            }) if source.is_none() => runtime.test_provider(id, provider, model),
            Ok(Command::ReloadConnectors) => runtime.reload_connectors(),
            Ok(Command::LaunchUi) if source.is_none() => match attach_panel(&runtime, &tx) {
                Ok((child, output)) => {
                    panel = Some(child);
                    *panel_output.borrow_mut() = Some(output);
                    runtime.emit_state();
                }
                Err(err) => runtime.emit_error(format!("启动自定义界面失败: {err}")),
            },
            Ok(Command::UiReady) if source.is_some() => {
                send_owner(&json!({ "event": "ui_ready", "pid": source }));
            }
            Ok(Command::OpenSettings) if source.is_some() => {
                send_owner(&json!({ "event": "open_settings" }));
            }
            Ok(Command::SaveConfig { .. } | Command::TestProvider { .. }) => {
                runtime.emit_error("请在官方设置窗口编辑或测试模型配置");
            }
            Ok(Command::LaunchUi | Command::UiReady | Command::OpenSettings) => {}
            Err(err) => runtime.emit_error(format!("无效指令: {err}")),
        }
    }
}
