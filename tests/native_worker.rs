//! The native worker as a real child process: the record it leaves, the digest
//! it reports, and the processes and pipes it releases.

#![allow(clippy::significant_drop_tightening)] // a guard held for one test block is contention-free

mod support;

use eja::{
    adapters::{
        Checkpoints, Control, FileTools, Model, NativeRuntime, NativeWorker, OpenAiModel,
        ToolContext, ToolGateway, Worktrees,
    },
    aja::Limits,
    Assignment, Result, Worker,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use support::{control, State};

// The worker is a child process and the tests read this process's children, so
// one test at a time keeps that observation about this process alone.
static LOCK: Mutex<()> = Mutex::new(());

/// A workspace on disk. The shared fixture owns the directory; the native worker
/// canonicalizes its project before it starts, so the directory must exist first.
fn workspace(tag: &str) -> State {
    let state = State::new(tag);
    std::fs::create_dir_all(state.path()).expect("workspace");
    state
}

/// The worker under test, over one workspace and its own private state.
fn worker_for(
    state: &State,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolGateway>,
    limits: Limits,
) -> Arc<NativeWorker> {
    Arc::new(
        NativeWorker::new(
            env!("CARGO_BIN_EXE_eja"),
            state.path().join(".eja/ajas"),
            state.path(),
            model,
            tools,
        )
        .unwrap()
        .with_limits(limits)
        .unwrap(),
    )
}

/// The frozen manifest of one aja record in this workspace.
fn manifest_of(state: &State, id: u64) -> Value {
    serde_json::from_slice(
        &std::fs::read(state.path().join(format!(".eja/ajas/{id}/aja.json"))).unwrap(),
    )
    .unwrap()
}

fn assignment(id: u64) -> Assignment {
    Assignment {
        task_id: id,
        objective: format!("aja {id} work"),
        workspace: String::new(),
        issued_at: 123,
        acceptance: String::new(),
        input: String::new(),
        resume: None,
    }
}
fn final_message(artifacts: &[&str]) -> Value {
    json!({"role":"assistant","content":json!({"status":"done","summary":"finished","artifacts":artifacts,"evidence":[],"open_questions":[]}).to_string()})
}
#[allow(clippy::needless_pass_by_value)] // callers build the arguments inline
fn call(name: &str, args: Value) -> Value {
    json!({"id":format!("call_{name}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})
}
fn calls(mut items: Vec<Value>) -> Value {
    let mut ids = std::collections::HashSet::new();
    for (index, item) in items.iter_mut().enumerate() {
        let id = item["id"].as_str().unwrap().to_string();
        if !ids.insert(id.clone()) {
            item["id"] = json!(format!("{id}_{index}"));
        }
    }
    json!({"role":"assistant","content":null,"tool_calls":items})
}

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Value>>,
    seen: Mutex<Vec<Vec<Value>>>,
    controls: Mutex<Vec<Control>>,
    pids: Mutex<Vec<i32>>,
}
impl Script {
    fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            ..Self::default()
        }
    }
}
impl Model for Script {
    fn complete(
        &self,
        messages: &[Value],
        _: &[Value],
        _: &Limits,
        control: &Control,
        _json_mode: bool,
    ) -> Result<Value> {
        control.check()?;
        self.seen.lock().unwrap().push(messages.to_vec());
        self.controls.lock().unwrap().push(control.clone());
        self.pids.lock().unwrap().extend(worker_pids());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "unexpected model call".into())
    }
}

struct Queue(Mutex<VecDeque<Value>>);

impl Queue {
    fn new(replies: Vec<Value>) -> Self {
        Self(Mutex::new(replies.into()))
    }
}

impl Model for Queue {
    fn complete(
        &self,
        _: &[Value],
        _: &[Value],
        _: &Limits,
        control: &Control,
        _: bool,
    ) -> Result<Value> {
        control.check()?;
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "unexpected model call".into())
    }
}

fn worker_pids() -> Vec<i32> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let parent: u32 = words.next()?.parse().ok()?;
            (parent == std::process::id() && words.any(|w| w == "--aja-worker")).then_some(pid)
        })
        .collect()
}
fn assert_reaped(pids: &[i32]) {
    for pid in pids {
        assert_eq!(unsafe { libc::kill(*pid, 0) }, -1, "worker {pid} survived");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[test]
fn native_loop_calls_host_tools_and_seals_a_versioned_result() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("native-loop");
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call("write_file", json!({"path":"out.txt","text":"aja result"})),
            call("read_file", json!({"path":"out.txt"})),
        ]),
        final_message(&["out.txt"]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert!(
        digest.contains("world: shared: +1 created (out.txt)"),
        "{digest}"
    );
    assert_eq!(
        std::fs::read_to_string(f.path().join("out.txt")).unwrap(),
        "aja result",
        "the aja writes the workspace itself"
    );
    assert!(manifest_of(&f, 1)["checkpoint"].as_str().is_some());
    assert_eq!(manifest_of(&f, 1)["frozen"], true);
    assert_eq!(manifest_of(&f, 1)["context"], "context.jsonl");
    assert!(
        std::fs::metadata(f.path().join(".eja/ajas/1/aja.json"))
            .unwrap()
            .permissions()
            .readonly(),
        "the frozen manifest is read-only"
    );
    let context = std::fs::read_to_string(f.path().join(".eja/ajas/1/context.jsonl")).unwrap();
    for kind in ["start", "assistant", "tool", "finish"] {
        assert!(
            context.contains(&format!("\"kind\":\"{kind}\"")),
            "context is missing {kind}: {context}"
        );
    }
    assert!(context.contains("aja result"));
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen[0][1]["content"], "aja 1 work");
    assert_eq!(seen[1][3]["role"], "tool");
    let read_result = seen[1]
        .iter()
        .find(|m| m["tool_call_id"] == "call_read_file")
        .unwrap();
    assert!(read_result["content"]
        .as_str()
        .unwrap()
        .contains("aja result"));
    assert!(model
        .controls
        .lock()
        .unwrap()
        .iter()
        .all(|c| c.check().is_err()));
    assert_eq!(w.active_count(), 0);
    assert!(!w.cancel(1));
    assert!(!model.pids.lock().unwrap().is_empty());
    assert_reaped(&model.pids.lock().unwrap());
    let before = manifest_of(&f, 1);
    assert!(w
        .execute(&assignment(1), &control())
        .contains("ended: error"));
    assert_eq!(manifest_of(&f, 1), before);
}

#[test]
fn a_worktree_run_persists_the_environment_and_stable_artifact_it_settled() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = workspace("native-world-record");
    let state_dir = fixture.path().join(".eja");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(fixture.path().join("base.txt"), "base\n").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "base.txt"],
        vec!["commit", "--quiet", "-m", "base"],
    ] {
        let status = Command::new("git")
            .current_dir(fixture.path())
            .args(["-c", "user.name=eja", "-c", "user.email=eja@local"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let control = control();
    let worlds = Arc::new(
        Worktrees::new(
            fixture.path(),
            state_dir.join("worlds"),
            &state_dir,
            &control,
        )
        .unwrap(),
    );
    let model = Arc::new(Queue::new(vec![
        calls(vec![call(
            "write_file",
            json!({"path":"out.txt","text":"isolated result"}),
        )]),
        final_message(&["out.txt"]),
    ]));
    let worker = NativeWorker::with_runtime(
        env!("CARGO_BIN_EXE_eja"),
        state_dir.join("ajas"),
        model,
        Arc::new(FileTools),
        NativeRuntime {
            workspace: Some(fixture.path().to_path_buf()),
            snapshots: Some(Checkpoints::new(
                state_dir.join("checkpoints"),
                state_dir.clone(),
            )),
            worlds: Some(worlds),
        },
    )
    .unwrap();
    let digest = worker.execute(&assignment(1), &control);
    assert!(digest.contains("ended: message"), "{digest}");
    assert!(!fixture.path().join("out.txt").exists());
    let manifest = manifest_of(&fixture, 1);
    assert_eq!(manifest["world_kind"], "worktree", "{manifest}");
    assert_eq!(
        manifest["world_root"],
        fixture.path().canonicalize().unwrap().display().to_string()
    );
    assert!(
        manifest["world_branch"]
            .as_str()
            .is_some_and(|branch| branch.starts_with("eja/aja-1-")),
        "{manifest}"
    );
    assert!(
        manifest["world_commit"]
            .as_str()
            .is_some_and(|commit| commit.len() == 40 || commit.len() == 64),
        "{manifest}"
    );
    assert!(manifest["checkpoint"].as_str().is_some(), "{manifest}");
}

#[test]
fn shell_glob_and_grep_work_inside_the_world() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("shell-tools");
    std::fs::create_dir_all(f.path().join("src")).unwrap();
    std::fs::write(
        f.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    std::fs::write(f.path().join("notes.md"), "remember the shell\n").unwrap();
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call(
                "shell",
                json!({"command":"printf ran > shell.txt && pwd && exit 3"}),
            ),
            call("glob", json!({"pattern":"**/*.rs"})),
            call("grep", json!({"pattern":"pub fn","include":"*.rs"})),
        ]),
        final_message(&["shell.txt"]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert_eq!(
        std::fs::read_to_string(f.path().join("shell.txt")).unwrap(),
        "ran"
    );
    let seen = model.seen.lock().unwrap();
    let tool = |id: &str| -> Value {
        let content = seen[1].iter().find(|m| m["tool_call_id"] == id).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned();
        let view: Value = serde_json::from_str(&content).unwrap();
        view["result"].clone()
    };
    let shell = tool("call_shell");
    assert_eq!(
        shell["exit_code"], 3,
        "a non-zero exit is an answer: {shell}"
    );
    let cwd = f.path().canonicalize().unwrap();
    assert!(shell["stdout"]
        .as_str()
        .unwrap()
        .contains(&cwd.display().to_string()));
    assert_eq!(shell["ended"], Value::Null);
    let glob = tool("call_glob");
    assert_eq!(glob["matches"], json!(["src/lib.rs"]), "{glob}");
    let grep = tool("call_grep");
    assert!(
        grep["matches"][0]
            .as_str()
            .unwrap()
            .starts_with("src/lib.rs:1:"),
        "{grep}"
    );
}

#[test]
fn a_shell_command_dies_with_its_run() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("shell-deadline");
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "shell",
            json!({"command":"sleep 30","timeout_ms":300}),
        )]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let started = Instant::now();
    let digest = w.execute(&assignment(1), &control());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline killed the sleep"
    );
    assert!(digest.contains("ended: message"), "{digest}");
    let seen = model.seen.lock().unwrap();
    let content = seen[1]
        .iter()
        .find(|m| m["tool_call_id"] == "call_shell")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let shell: Value = serde_json::from_str(&content).unwrap();
    assert!(
        shell["result"]["ended"]
            .as_str()
            .unwrap()
            .contains("timed out"),
        "{shell}"
    );
    assert!(!model.pids.lock().unwrap().is_empty());
    assert_reaped(&model.pids.lock().unwrap());
}

/// Effects that reach past the work area, so the digest can be checked against
/// the world rather than the aja's own account.
struct Effects;
impl ToolGateway for Effects {
    fn owns(&self, name: &str) -> bool {
        name == "effect"
    }
    fn definitions(&self, _: &Assignment) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"effect","description":"apply one effect","parameters":{"type":"object","properties":{"kind":{"type":"string"},"path":{"type":"string"}},"required":["kind","path"]}}}),
        ]
    }
    fn call(&self, c: &ToolContext<'_>, _: &str, args: &Value) -> Result<Value> {
        let path = args["path"].as_str().unwrap_or_default();
        match args["kind"].as_str().unwrap_or_default() {
            "project" => {
                let target = c.workspace()?.join(path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, "delivered")?;
                Ok(json!({"ok": true}))
            }
            "drop" => {
                std::fs::remove_file(c.workspace()?.join(path))?;
                Ok(json!({"ok": true}))
            }
            other => Err(format!("unknown effect {other}").into()),
        }
    }
}

#[test]
fn the_digest_reports_what_the_project_gained() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("digest");
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call("effect", json!({"kind":"project","path":"delivered.txt"})),
            call("effect", json!({"kind":"project","path":"nested/also.txt"})),
        ]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model, Arc::new(Effects), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(
        digest.contains("world: shared: +2 created (delivered.txt, nested/also.txt)"),
        "{digest}"
    );
}

#[test]
fn the_aja_leaves_a_fossil_of_the_world_it_ended_in() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("fossil");
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "effect",
            json!({"kind":"project","path":"report.md"}),
        )]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model, Arc::new(Effects), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("+1 created"), "{digest}");
    let manifest = manifest_of(&f, 1);
    let commit = manifest["checkpoint"]
        .as_str()
        .expect("a aja records its checkpoint");
    assert_eq!(commit.len(), 40, "{commit}");
    let read = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(f.path().join(".eja/checkpoints"))
        .arg("show")
        .arg(format!("{commit}:report.md"))
        .output()
        .expect("git");
    assert_eq!(String::from_utf8_lossy(&read.stdout), "delivered");
}

/// A gateway that answers with something far larger than a aja should carry.
struct Big;
impl ToolGateway for Big {
    fn owns(&self, name: &str) -> bool {
        name == "big"
    }
    fn definitions(&self, _: &Assignment) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"big","description":"a big result","parameters":{"type":"object","properties":{}}}}),
        ]
    }
    fn call(&self, _: &ToolContext<'_>, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({"text": "x".repeat(64 * 1024)}))
    }
}

#[test]
fn a_large_tool_result_is_a_view_in_the_transcript_and_whole_in_the_record() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("large-result");
    let model = Arc::new(Script::new(vec![
        calls(vec![call("big", json!({}))]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(Big), Limits::default());
    w.execute(&assignment(1), &control());
    let record = std::fs::read_to_string(f.path().join(".eja/ajas/1/context.jsonl")).unwrap();
    assert!(
        record.contains(&"x".repeat(60 * 1024)),
        "the record keeps it whole"
    );
    let seen = model.seen.lock().unwrap();
    let carried = seen[1]
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the aja saw a tool result");
    assert!(carried.contains("truncated"), "{carried:.120}");
    assert!(carried.contains("the full result stays in the record"));
    assert!(
        carried.len() < 16 * 1024,
        "the view is bounded: {}",
        carried.len()
    );
}

#[test]
fn a_continuation_starts_from_the_predecessor_record() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("continuation");
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "effect",
            json!({"kind":"project","path":"first.md"}),
        )]),
        final_message(&[]),
        calls(vec![call(
            "effect",
            json!({"kind":"project","path":"second.md"}),
        )]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(Effects), Limits::default());
    w.execute(&assignment(1), &control());
    let mut second = assignment(2);
    second.resume = Some(1);
    w.execute(&second, &control());
    let seen = model.seen.lock().unwrap();
    let first_call_of_second_aja = &seen[2];
    assert!(
        first_call_of_second_aja[0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("You are one AJA")),
        "the inherited turn opens with the current system contract"
    );
    assert!(
        first_call_of_second_aja
            .iter()
            .any(|message| message["role"] == "tool"),
        "the inherited turn carries its tool result"
    );
    let brief = first_call_of_second_aja
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default();
    assert!(brief.starts_with("Continuing as a new aja."), "{brief}");
}

struct Policy;
impl ToolGateway for Policy {
    fn owns(&self, name: &str) -> bool {
        FileTools.owns(name)
    }
    fn definitions(&self, a: &Assignment) -> Vec<Value> {
        FileTools
            .definitions(a)
            .into_iter()
            .filter(|d| a.task_id != 2 || d["function"]["name"] == "read_file")
            .collect()
    }
    fn call(&self, c: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        FileTools.call(c, name, args)
    }
}

#[test]
fn ajas_share_the_workspace_and_grants_still_differ() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("shared-workspace");
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "write_file",
            json!({"path":"same.txt","text":"one"}),
        )]),
        final_message(&[]),
        calls(vec![
            call("read_file", json!({"path":"same.txt"})),
            call("write_file", json!({"path":"same.txt","text":"two"})),
        ]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(Policy), Limits::default());
    assert!(w
        .execute(&assignment(1), &control())
        .contains("ended: message"));
    assert!(w
        .execute(&assignment(2), &control())
        .contains("ended: message"));
    let seen = model.seen.lock().unwrap();
    let results = seen[3]
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    // There is one workspace: the second aja reads what the first wrote.
    assert!(results[0].contains("one"), "{}", results[0]);
    // The grant, not the filesystem, is what a aja may not cross.
    assert!(results[1].contains("not allowed"), "{}", results[1]);
    assert_eq!(
        std::fs::read_to_string(f.path().join("same.txt")).unwrap(),
        "one"
    );
}

#[test]
fn traversal_private_state_and_symlinks_are_not_file_capabilities() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("traversal");
    std::fs::create_dir_all(f.path().join(".eja")).unwrap();
    std::fs::write(f.path().join(".eja/secret.txt"), "private").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(f.path().join(".eja/secret.txt"), f.path().join("alias.txt"))
        .unwrap();
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call("write_file", json!({"path":"../escape.txt","text":"bad"})),
            call("read_file", json!({"path":".eja/secret.txt"})),
            call("read_file", json!({"path":"alias.txt"})),
        ]),
        final_message(&["../escape.txt"]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    // The claim mentions an escaping path; the world did not move.
    assert!(digest.contains("world: shared: unchanged"), "{digest}");
    assert!(!f.path().join("escape.txt").exists());
    assert_eq!(manifest_of(&f, 1)["frozen"], true);
    assert_eq!(
        model.seen.lock().unwrap()[1]
            .iter()
            .filter(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("error"))
            .count(),
        3
    );
}

#[test]
fn step_context_and_output_limits_stop_the_aja_and_preserve_partial_work() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("limits");
    let model = Arc::new(Script::new(vec![calls(vec![call(
        "write_file",
        json!({"path":"partial.txt","text":"kept"}),
    )])]));
    let w = worker_for(
        &f,
        model,
        Arc::new(FileTools),
        Limits {
            steps: Some(1),
            ..Limits::default()
        },
    );
    let r = w.execute(&assignment(1), &control());
    // A spent budget stops the aja without calling its work a failure.
    assert!(r.contains("ended: budget"), "{r}");
    assert!(r.contains("budget"), "{r}");
    assert!(r.contains("world: shared: +1 created (partial.txt)"), "{r}");
    assert!(manifest_of(&f, 1)["digest"]
        .as_str()
        .unwrap()
        .contains("ended: budget"));
    let context_limits = Limits {
        context_bytes: 16,
        ..Limits::default()
    };
    let context_model = Arc::new(Script::new(vec![]));
    let w = worker_for(&f, context_model, Arc::new(FileTools), context_limits);
    let r = w.execute(&assignment(2), &control());
    assert!(r.contains("ended: budget"), "{r}");
    assert!(r.contains("context budget"), "{r}");
    assert_eq!(w.active_count(), 0);
    assert_eq!(manifest_of(&f, 2)["frozen"], true);
    // A protocol breach is an error, not a budget the aja spent.
    let oversized = Arc::new(Script::new(vec![json!({
        "role": "assistant",
        "content": "x".repeat(129),
    })]));
    let w = worker_for(
        &f,
        oversized,
        Arc::new(FileTools),
        Limits {
            response_bytes: 128,
            ..Limits::default()
        },
    );
    let r = w.execute(&assignment(3), &control());
    assert!(r.contains("ended: error"), "{r}");
    assert!(r.contains("output limit"), "{r}");
    assert_eq!(w.active_count(), 0);
    assert_eq!(manifest_of(&f, 3)["frozen"], true);
    // A plain-text final message is a claim, not a schema breach.
    let plain = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":"not a digest"}),
    ]));
    let w = worker_for(&f, plain.clone(), Arc::new(FileTools), Limits::default());
    let r = w.execute(&assignment(5), &control());
    assert!(r.contains("ended: message"), "{r}");
    assert!(r.contains("not a digest"), "{r}");
    assert_reaped(&plain.pids.lock().unwrap());
}

struct Blocking(Arc<AtomicBool>);
impl Model for Blocking {
    fn complete(
        &self,
        _: &[Value],
        _: &[Value],
        _: &Limits,
        c: &Control,
        _: bool,
    ) -> Result<Value> {
        self.0.store(true, Ordering::SeqCst);
        loop {
            c.check()?;
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn timeout_cancellation_and_busy_slot_release_all_aja_resources() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for cancel in [false, true] {
        let f = workspace("timeout");
        let started = Arc::new(AtomicBool::new(false));
        let w = worker_for(
            &f,
            Arc::new(Blocking(started.clone())),
            Arc::new(FileTools),
            Limits {
                timeout_ms: if cancel { 5000 } else { 100 },
                ..Limits::default()
            },
        );
        let running = w.clone();
        let join = std::thread::spawn(move || running.execute(&assignment(1), &control()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let pids = worker_pids();
        assert!(!pids.is_empty());
        // One shared world has one owner: the second run is refused by the world,
        // not by a slot counter in the worker, and it costs no aja id.
        let refused = w.execute(&assignment(2), &control());
        assert!(refused.contains("ended: error"), "{refused}");
        assert!(refused.contains("one owner"), "{refused}");
        assert!(!f.path().join(".eja/ajas/2").exists());
        if cancel {
            assert!(w.cancel(1));
        }
        let r = join.join().unwrap();
        assert!(r.contains("ended: budget"), "{r}");
        assert!(
            r.contains(if cancel { "cancelled" } else { "timed out" }),
            "{r}"
        );
        assert_eq!(w.active_count(), 0);
        assert_eq!(manifest_of(&f, 1)["frozen"], true);
        assert_reaped(&pids);
    }
}

#[test]
fn a_aja_stopped_mid_run_claims_no_invented_project_change() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Cancelled while the model is working.
    let f = workspace("stopped");
    std::fs::write(f.path().join("kept.txt"), "work").unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let w = worker_for(
        &f,
        Arc::new(Blocking(started.clone())),
        Arc::new(FileTools),
        Limits::default(),
    );
    let running = w.clone();
    let join = std::thread::spawn(move || running.execute(&assignment(1), &control()));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(w.cancel(1));
    let digest = join.join().unwrap();
    assert!(digest.contains("ended: budget"), "{digest}");
    // A cancelled run still gets a measured account, and never an invented one:
    // it can report what the world holds, never a change it did not observe.
    assert!(!digest.contains("deleted"), "{digest}");
    assert!(!digest.contains("created"), "{digest}");
    assert!(
        digest.contains("unchanged") || digest.contains("inventory truncated"),
        "{digest}"
    );
    // A turn deadline that expires before the aja could do anything.
    let g = workspace("stopped-deadline");
    std::fs::write(g.path().join("kept.txt"), "work").unwrap();
    let w = worker_for(
        &g,
        Arc::new(Blocking(Arc::new(AtomicBool::new(false)))),
        Arc::new(FileTools),
        Limits::default(),
    );
    let parent = Control::new(Duration::from_millis(1), Arc::new(AtomicBool::new(false)));
    let digest = w.execute(&assignment(1), &parent);
    assert!(!digest.contains("deleted"), "{digest}");
    assert!(!digest.contains("created"), "{digest}");
}

#[test]
fn many_frozen_ajas_have_no_live_workers_or_growing_pipe_handles() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("many-frozen");
    let model = Arc::new(Script::new((0..25).map(|_| final_message(&[])).collect()));
    let w = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let fd_path = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let before = std::fs::read_dir(fd_path).unwrap().count();
    for id in 1..=25 {
        assert!(w
            .execute(&assignment(id), &control())
            .contains("ended: message"));
        assert_eq!(w.active_count(), 0);
    }
    assert!(std::fs::read_dir(fd_path).unwrap().count() <= before + 1);
    assert!(worker_pids().is_empty());
    assert_reaped(&model.pids.lock().unwrap());
    assert_eq!(
        std::fs::read_dir(f.path().join(".eja/ajas"))
            .unwrap()
            .count(),
        25
    );
}

#[test]
fn native_worker_exits_when_its_host_pipe_disappears() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut child = Command::new(env!("CARGO_BIN_EXE_eja"))
        .arg("--aja-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = libc::pid_t::try_from(child.id()).expect("a pid fits a pid_t");
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"context_bytes":Limits::default().context_bytes,"messages":[{"role":"system","content":"You are AJA"},{"role":"user","content":"work"}]})
    )
    .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut request = String::new();
    output.read_line(&mut request).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&request).unwrap()["op"],
        "model"
    );
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("worker survived host disconnect");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_reaped(&[pid]);
}

fn serve(responses: Vec<Value>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            seen.push(serde_json::from_slice(&body).unwrap());
            let body = response.to_string();
            let _=write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
        }
        seen
    });
    (url, server)
}

#[test]
fn real_http_transport_drives_the_native_worker_tool_round_trip() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("real-http");
    let (url, server) = serve(vec![
        json!({"choices":[{"finish_reason":"tool_calls","message":calls(vec![call("write_file",json!({"path":"http.txt","text":"real host round trip"}))])}]}),
        json!({"choices":[{"finish_reason":"stop","message":final_message(&["http.txt"])}]}),
    ]);
    let w = worker_for(
        &f,
        Arc::new(OpenAiModel::new(url, "test-key", "test-model")),
        Arc::new(FileTools),
        Limits::default(),
    );
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"), "{digest}");
    let seen = server.join().unwrap();
    assert_eq!(seen[0]["messages"][1]["content"], "aja 1 work");
    assert_eq!(seen[0]["tools"].as_array().unwrap().len(), 8);
    assert!(seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool"));
    assert!(
        digest.contains("world: shared: +1 created (http.txt)"),
        "{digest}"
    );
}

#[test]
fn transport_rejects_truncation_duplicate_calls_and_oversized_responses() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for response in [
        json!({"choices":[{"finish_reason":"length","message":final_message(&[])}]}),
        json!({"choices":[{"finish_reason":"stop","message":null}]}),
        json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[call("read_file",json!({})),call("read_file",json!({}))]}}]}),
        json!({"choices":[{"finish_reason":"stop","message":{"content":"x".repeat(2048)}}]}),
    ] {
        let (url, server) = serve(vec![response]);
        let model = OpenAiModel::new(url, "test-key", "test-model");
        let control = Control::new(Duration::from_secs(3), Arc::new(AtomicBool::new(false)));
        assert!(model
            .complete(
                &[json!({"role":"user","content":"work"})],
                &[],
                &Limits {
                    response_bytes: 1024,
                    ..Limits::default()
                },
                &control,
                false
            )
            .is_err());
        server.join().unwrap();
    }
}

#[test]
fn cancellation_interrupts_a_stalled_real_http_request() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = [0; 8192];
        assert!(stream.read(&mut bytes).unwrap() > 0);
        started_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    let flag = Arc::new(AtomicBool::new(false));
    let control = Control::new(Duration::from_secs(5), flag.clone());
    let running = std::thread::spawn(move || {
        OpenAiModel::new(url, "test-key", "test-model")
            .complete(
                &[json!({"role":"user","content":"work"})],
                &[],
                &Limits::default(),
                &control,
                false,
            )
            .map_err(|e| e.to_string())
    });
    started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let at = Instant::now();
    flag.store(true, Ordering::SeqCst);
    let result = running.join().unwrap();
    assert!(result.unwrap_err().contains("cancelled"));
    assert!(at.elapsed() < Duration::from_secs(1));
    release_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn oversized_start_is_rejected_without_leaving_a_process() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("oversized-start");
    let model = Arc::new(Script::default());
    let worker = worker_for(&f, model, Arc::new(FileTools), Limits::default());
    let mut input = assignment(1);
    input.objective = "x".repeat(eja::aja::FRAME_LIMIT);
    let digest = worker.execute(&input, &control());
    assert!(digest.contains("ended: budget"), "{digest}");
    assert!(digest.contains("context budget"), "{digest}");
    assert_eq!(worker.active_count(), 0);
    assert!(worker_pids().is_empty());
    assert_eq!(manifest_of(&f, 1)["frozen"], true);
}

struct TestRuntime(Child);
impl Drop for TestRuntime {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn post_detached_turn(mut command: Command) -> String {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _runtime = TestRuntime(
        command
            .args(["--port", &port.to_string()])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            break stream;
        }
        assert!(Instant::now() < deadline, "runtime did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body = r#"{"text":"run the MCP fixture"}"#;
    write!(
        stream,
        "POST /api/turn HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    response
}

#[test]
fn cli_and_http_complete_mcp_work_without_a_project_or_git() {
    use eja::adapters::{RecordFiles, RecordReader};
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for via_http in [false, true] {
        let fixture = workspace("connector-only");
        let state = fixture.path().join(".eja");
        let old_worktree = state.join("worlds/1");
        std::fs::create_dir_all(&old_worktree).unwrap();
        std::fs::write(old_worktree.join("existing.txt"), "old worktree").unwrap();
        let peer = fixture.path().join("peer");
        let empty_path = fixture.path().join("bin");
        std::fs::create_dir_all(&peer).unwrap();
        std::fs::create_dir_all(&empty_path).unwrap();
        std::fs::write(state.join("mcp.json"), json!({"mcpServers":{"fixture":{
            "command":"/usr/bin/python3",
            "args":[PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_server.py"), peer, "stdio"],
            "tools":["echo"]
        }}}).to_string()).unwrap();

        let (url, server) = serve(vec![
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":json!({"actions":[
                {"op":"open_task","title":"connector only","request":"use the echo tool","acceptance":"it returned","run":true}
            ]}).to_string()}}]}),
            json!({"choices":[{"finish_reason":"tool_calls","message":calls(vec![
                call("mcp__fixture__echo", json!({"text":"fixture-returned"}))
            ])}]}),
            json!({"choices":[{"finish_reason":"stop","message":final_message(&[])}]}),
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":json!({"reply":"Accepted the echo.","actions":[
                {"op":"close_task","task_id":1,"accepted":true}
            ]}).to_string()}}]}),
        ]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_eja"));
        command
            .current_dir(fixture.path())
            .env("PATH", &empty_path)
            .env("LNA_BASE_URL", url)
            .env("LNA_API_KEY", "test-key")
            .env("LNA_MODEL", "test-model");
        if via_http {
            command.arg("serve");
        }
        command.args(["--no-project", "--dir", state.to_str().unwrap()]);
        let reply = if via_http {
            post_detached_turn(command)
        } else {
            let output = command
                .args(["turn", "run the MCP fixture"])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        assert!(reply.contains("Accepted the echo."), "{reply}");
        let requests = server.join().unwrap();
        let names: Vec<_> = requests[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["mcp__fixture__echo", "eja_read_record"]);
        let reopened = eja::State::load(&eja::Store::new(&state).unwrap()).unwrap();
        let task = reopened.task(1).unwrap();
        assert_eq!(task.status, eja::TaskStatus::Done);
        assert!(task.workspace.is_empty());
        let manifest = manifest_of(&fixture, 1);
        assert_eq!(manifest["frozen"], true);
        assert!(manifest["checkpoint"].is_null() && manifest["checkpoint_error"].is_null());
        let record = RecordFiles::with_checkpoints(state.join("ajas"), state.join("checkpoints"))
            .slice("1/context.jsonl", &eja::Selector::Whole, &control())
            .unwrap();
        assert!(record["text"]
            .as_str()
            .unwrap()
            .contains("fixture-returned"));
        assert!(!state.join("checkpoints").exists());
        assert!(old_worktree.join("existing.txt").is_file());
    }
}

#[test]
fn a_workspace_can_write_files_without_shell_or_git() {
    use std::os::unix::fs::PermissionsExt as _;

    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fixture = workspace("workspace-without-git");
    let bin = fixture.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let marker = fixture.path().join("git-invoked");
    let git = bin.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\nprintf 'called' > \"$TEST_GIT_MARKER\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();

    let (url, server) = serve(vec![
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":json!({"actions":[
            {"op":"open_task","title":"local file","request":"write local.txt","acceptance":"file written","run":true}
        ]}).to_string()}}]}),
        json!({"choices":[{"finish_reason":"tool_calls","message":calls(vec![
            call("write_file", json!({"path":"local.txt","text":"local result"}))
        ])}]}),
        json!({"choices":[{"finish_reason":"stop","message":final_message(&[])}]}),
        json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"{\"reply\":\"recorded\"}"}}]}),
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .current_dir(fixture.path())
        .env("PATH", bin)
        .env("TEST_GIT_MARKER", &marker)
        .env("EJA_CAPABILITIES", "workspace,files")
        .env("EJA_AJA_SLOTS", "2")
        .env("LNA_BASE_URL", url)
        .env("LNA_API_KEY", "test-key")
        .env("LNA_MODEL", "test-model")
        .args(["--dir", ".eja", "turn", "write a local file"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("recorded"),
        "{output:?}"
    );
    let requests = server.join().unwrap();
    let tools = requests[1]["tools"].as_array().unwrap();
    assert!(tools
        .iter()
        .any(|tool| tool["function"]["name"] == "write_file"));
    assert!(tools.iter().all(|tool| tool["function"]["name"] != "shell"));
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("local.txt")).unwrap(),
        "local result"
    );
    assert!(!marker.exists());
    let manifest = manifest_of(&fixture, 1);
    assert!(manifest["checkpoint"].is_null() && manifest["checkpoint_error"].is_null());

    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .current_dir(fixture.path())
        .env("EJA_CAPABILITIES", "workspace,shell")
        .args(["--dir", ".eja", "show", "tools"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let tools: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "shell");
}

#[test]
fn cli_eja_dispatches_native_aja_reads_frozen_output_and_accepts() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("cli");
    let fixture = f.path().join(".eja/fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(f.path().join(".eja/mcp.json"), json!({"mcpServers":{"files":{
        "command":"/usr/bin/python3",
        "args":[PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_server.py"),fixture,"files"],
        "tools":["write_file"]
    }}}).to_string()).unwrap();
    let decision = |reply: &str, actions: Value| json!({"role":"assistant","content":json!({"reply":reply,"actions":actions}).to_string()});
    let response = |reason: &str, message: Value| json!({"choices":[{"finish_reason":reason,"message":message}]});
    let (url, server) = serve(vec![
        response(
            "stop",
            decision(
                "working",
                json!([{"op":"open_task","title":"checklist","request":"write only the release checklist","acceptance":"build and test","run":true}]),
            ),
        ),
        response(
            "tool_calls",
            calls(vec![call(
                "mcp__files__write_file",
                json!({"path":"checklist.txt","text":"build\ntest"}),
            )]),
        ),
        response("stop", final_message(&["checklist.txt"])),
        response(
            "stop",
            decision(
                "reading",
                json!([{"op":"look","target":"record:1/checklist.txt"}]),
            ),
        ),
        response(
            "stop",
            decision(
                "accepted",
                json!([{"op":"close_task","task_id":1,"accepted":true,"note":"read build and test"}]),
            ),
        ),
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .current_dir(f.path())
        .env("LNA_BASE_URL", url)
        .env("LNA_API_KEY", "test-key")
        .env("LNA_MODEL", "test-model")
        .args(["--dir", ".eja", "turn", "broad original user request"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("eja: accepted"));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[1]["messages"][1]["content"],
        "write only the release checklist\n\nTask as given:\nbroad original user request\n\nAcceptance: build and test"
    );
    assert!(requests[4]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("build"));
    assert_eq!(manifest_of(&f, 1)["frozen"], true);
    let log = std::fs::read_to_string(f.path().join(".eja/log.jsonl")).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(events
        .iter()
        .any(|e| e["kind"] == "task_closed" && e["data"]["accepted"] == true));
    let observed = events.iter().find(|e| e["kind"] == "observed").unwrap();
    assert!(observed["data"]["error"].is_null(), "{observed}");
    assert_eq!(observed["data"]["result"]["text"], "build\ntest");
    assert!(!log.contains("test-key"));
    assert!(worker_pids().is_empty());
}

#[test]
fn live_and_replayed_results_share_one_view_and_large_records_still_resume() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("live-replay");
    let mut many = Vec::new();
    for index in 0..5 {
        let mut item = call("big", json!({}));
        item["id"] = json!(format!("big_{index}"));
        many.push(item);
    }
    let model = Arc::new(Script::new(vec![
        calls(many),
        final_message(&[]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(Big), Limits::default());
    let first = w.execute(&assignment(1), &control());
    assert!(first.contains("ended: message"), "{first}");
    let mut next = assignment(2);
    next.resume = Some(1);
    let second = w.execute(&next, &control());
    assert!(second.contains("ended: message"), "{second}");
    let seen = model.seen.lock().unwrap();
    let live: Vec<_> = seen[1]
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    let inherited: Vec<_> = seen[2]
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(
        live, inherited,
        "replay must use the same result views as live execution"
    );
    assert!(serde_json::to_vec(&seen[2]).unwrap().len() <= Limits::default().context_bytes);
    let record = std::fs::read_to_string(f.path().join(".eja/ajas/2/context.jsonl")).unwrap();
    assert!(
        !record.contains(&"x".repeat(1000)),
        "a continuation stores a parent reference, not a copied payload"
    );
    assert!(record.len() < 5000, "{}", record.len());
}

struct Passage;
impl ToolGateway for Passage {
    fn owns(&self, name: &str) -> bool {
        name == "big"
    }
    fn definitions(&self, _: &Assignment) -> Vec<Value> {
        Big.definitions(&assignment(1))
    }
    fn call(&self, _: &ToolContext<'_>, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({"text":format!("{}MIDDLE-SECRET-77{}","前".repeat(12000),"后".repeat(12000))}))
    }
}
struct ReadPassage(Mutex<usize>);
impl Model for ReadPassage {
    fn complete(
        &self,
        messages: &[Value],
        _: &[Value],
        _: &Limits,
        _: &Control,
        _: bool,
    ) -> Result<Value> {
        let mut step = self.0.lock().unwrap();
        let result = match *step {
            0 => calls(vec![call("big", json!({}))]),
            1 => {
                let content = messages
                    .iter()
                    .find(|message| message["role"] == "tool")
                    .unwrap()["content"]
                    .as_str()
                    .unwrap();
                assert!(!content.contains("MIDDLE-SECRET-77"));
                let view: Value = serde_json::from_str(content).unwrap();
                calls(vec![call(
                    "eja_read_record",
                    json!({"aja":view["record"]["aja"],"entry":view["record"]["entry"],"offset":35980,"bytes":128}),
                )])
            }
            2 => {
                assert!(messages.iter().any(|message| message["role"] == "tool"
                    && message["content"]
                        .as_str()
                        .unwrap()
                        .contains("MIDDLE-SECRET-77")));
                final_message(&[])
            }
            _ => return Err("unexpected call".into()),
        };
        *step += 1;
        Ok(result)
    }
}
#[test]
fn omitted_tool_passages_remain_addressable_without_expanding_grants() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("passage");
    let w = worker_for(
        &f,
        Arc::new(ReadPassage(Mutex::new(0))),
        Arc::new(Passage),
        Limits::default(),
    );
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert!(digest.contains("eja_read_recordx1"));
}

#[test]
fn checkpoint_whole_range_and_match_resolve_the_same_utf8_file() {
    use eja::{
        adapters::{RecordFiles, RecordReader},
        Selector,
    };
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("checkpoint");
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "write_file",
            json!({"path":"out.txt","text":"第一行\n第二行\n第三行\n"}),
        )]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model, Arc::new(FileTools), Limits::default());
    assert!(w
        .execute(&assignment(1), &control())
        .contains("ended: message"));
    let reader = RecordFiles::with_checkpoints(
        f.path().join(".eja/ajas"),
        f.path().join(".eja/checkpoints"),
    );
    assert!(reader
        .slice("1/out.txt", &eja::Selector::Whole, &control())
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("第二行"));
    let range = reader
        .slice(
            "1/out.txt",
            &Selector::Range { from: 1, lines: 1 },
            &control(),
        )
        .unwrap();
    assert_eq!(range["lines"][0]["text"], "第二行");
    let found = reader
        .slice(
            "1/out.txt",
            &Selector::Match {
                text: "第二行".into(),
                before: 1,
                after: 1,
            },
            &control(),
        )
        .unwrap();
    assert_eq!(found["matches"][0]["context"].as_array().unwrap().len(), 3);
}

#[test]
fn a_continuation_keeps_the_complete_prefix_of_an_interrupted_call_group() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("prefix");
    // The host records the assistant call before an over-sized model response
    // can be sent over IPC. Simulate a frozen, interrupted record explicitly.
    let aja = f.path().join(".eja/ajas/1");
    std::fs::create_dir_all(&aja).unwrap();
    let start = json!({"kind":"start","view_version":1,"messages":[{"role":"system","content":"You are AJA"},{"role":"user","content":"old brief"}]});
    let completed =
        json!({"kind":"assistant","message":{"role":"assistant","content":"completed-prefix"}});
    let unfinished = json!({"kind":"assistant","message":calls(vec![call("big",json!({}))])});
    std::fs::write(
        aja.join("context.jsonl"),
        format!("{start}\n{completed}\n{unfinished}\n"),
    )
    .unwrap();
    std::fs::write(
        aja.join("aja.json"),
        json!({"assignment":assignment(1),"frozen":true}).to_string(),
    )
    .unwrap();
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let w = worker_for(&f, model.clone(), Arc::new(Big), Limits::default());
    let mut next = assignment(2);
    next.resume = Some(1);
    assert!(w.execute(&next, &control()).contains("ended: message"));
    let seen = model.seen.lock().unwrap();
    assert!(seen[0]
        .iter()
        .any(|message| message["content"] == "completed-prefix"));
    assert!(!seen[0]
        .iter()
        .any(|message| message.get("tool_calls").is_some()));
}

#[test]
fn a_turn_deadline_cannot_be_extended_by_a_ajas_own_timeout() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("deadline");
    let started = Arc::new(AtomicBool::new(false));
    let w = worker_for(
        &f,
        Arc::new(Blocking(started)),
        Arc::new(Big),
        Limits {
            timeout_ms: 5000,
            ..Limits::default()
        },
    );
    let parent = Control::new(Duration::from_millis(100), Arc::new(AtomicBool::new(false)));
    let began = Instant::now();
    let digest = w.execute(&assignment(1), &parent);
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(digest.contains("ended: budget"), "{digest}");
    assert!(digest.contains("timed out"));
    assert_eq!(w.active_count(), 0);
    assert_eq!(manifest_of(&f, 1)["frozen"], true);
    assert!(manifest_of(&f, 1)["checkpoint_error"]
        .as_str()
        .unwrap()
        .contains("timed out"));
}

#[test]
fn record_addresses_cannot_expand_a_ajas_ancestry_grant() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("ancestry");
    let model = Arc::new(Script::new(vec![
        calls(vec![call("eja_read_record", json!({"aja":999,"entry":0}))]),
        final_message(&[]),
    ]));
    let w = worker_for(&f, model.clone(), Arc::new(Big), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"));
    let seen = model.seen.lock().unwrap();
    assert!(seen[1].iter().any(|message| message["role"] == "tool"
        && message["content"]
            .as_str()
            .unwrap()
            .contains("outside this aja's ancestry")));
}

#[test]
fn relative_private_root_is_canonical_and_excluded_from_project_evidence() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("relative-root");
    std::fs::create_dir_all(f.path().join(".eja")).unwrap();
    std::fs::write(f.path().join(".eja/secret"), "private-token").unwrap();
    std::fs::write(f.path().join("public.txt"), "public").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(f.path().strip_prefix("/").unwrap());
    relative.push(".eja/ajas");
    assert!(relative.is_relative());
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let worker = NativeWorker::new(
        env!("CARGO_BIN_EXE_eja"),
        relative,
        f.path(),
        model,
        Arc::new(FileTools),
    )
    .unwrap();
    let digest = worker.execute(&assignment(1), &control());
    assert!(digest.contains("world: shared: unchanged"), "{digest}");
    let manifest = manifest_of(&f, 1);
    let commit = manifest["checkpoint"].as_str().unwrap();
    let tree = Command::new("git")
        .arg("--git-dir")
        .arg(f.path().join(".eja/checkpoints"))
        .args(["ls-tree", "-r", "--name-only", commit])
        .output()
        .unwrap();
    assert!(tree.status.success());
    assert_eq!(String::from_utf8(tree.stdout).unwrap().trim(), "public.txt");
}

#[test]
fn a_valid_long_assignment_crosses_start_ipc_once() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("long-assignment");
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let worker = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let mut input = assignment(1);
    input.objective = "x".repeat(200 * 1024);
    let digest = worker.execute(&input, &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert_eq!(model.seen.lock().unwrap()[0][1]["content"], input.objective);
}

#[test]
fn malformed_custom_model_messages_are_recorded_as_errors_and_not_replayed() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("malformed");
    let model = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":null}),
        final_message(&[]),
    ]));
    let worker = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let first = worker.execute(&assignment(1), &control());
    assert!(first.contains("ended: error"), "{first}");
    let record = std::fs::read_to_string(f.path().join(".eja/ajas/1/context.jsonl")).unwrap();
    assert!(record.contains("model_error"));
    let mut next = assignment(2);
    next.resume = Some(1);
    let digest = worker.execute(&next, &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert!(model.seen.lock().unwrap()[1]
        .iter()
        .all(|m| m["role"] != "assistant"));
}

#[test]
fn cli_show_cannot_enter_turn_and_invalid_context_budget_is_explicit() {
    let f = workspace("cli-show");
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .args(["--dir", f.path().to_str().unwrap(), "show", "turn"])
        .env("LNA_BASE_URL", "http://127.0.0.1:1")
        .env("LNA_API_KEY", "test")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown show target"));
    assert!(!f.path().join("log.jsonl").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .current_dir(f.path())
        .args(["--dir", ".eja", "turn", "hi"])
        .env("LNA_BASE_URL", "http://127.0.0.1:1")
        .env("LNA_API_KEY", "test")
        .env("EJA_CONTEXT_BYTES", "0")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("EJA_CONTEXT_BYTES"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn empty_tool_lists_are_final_messages_and_a_missing_record_start_is_an_error() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("empty-calls");
    let model = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":"completed prefix","tool_calls":[]}),
        final_message(&[]),
    ]));
    let worker = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    assert!(worker
        .execute(&assignment(1), &control())
        .contains("ended: message"));
    let mut next = assignment(2);
    next.resume = Some(1);
    assert!(worker.execute(&next, &control()).contains("ended: message"));
    assert!(model.seen.lock().unwrap()[1]
        .iter()
        .any(|m| m["content"] == "completed prefix"));
    let damaged = f.path().join(".eja/ajas/3");
    std::fs::create_dir_all(&damaged).unwrap();
    std::fs::write(
        damaged.join("aja.json"),
        json!({"frozen":true,"assignment":assignment(3)}).to_string(),
    )
    .unwrap();
    std::fs::write(damaged.join("context.jsonl"), "").unwrap();
    let mut next = assignment(4);
    next.resume = Some(3);
    let digest = worker.execute(&next, &control());
    assert!(
        digest.contains("ended: error") && digest.contains("no start entry"),
        "{digest}"
    );
}

#[test]
fn a_continuation_refuses_a_predecessor_that_never_froze() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("unfrozen");
    let aja = f.path().join(".eja/ajas/1");
    std::fs::create_dir_all(&aja).unwrap();
    let start = json!({"kind":"start","view_version":1,"messages":[{"role":"system","content":"You are AJA"},{"role":"user","content":"old brief"}]});
    std::fs::write(aja.join("context.jsonl"), format!("{start}\n")).unwrap();
    // No `frozen`: the host never sealed this record, so it is not evidence.
    std::fs::write(
        aja.join("aja.json"),
        json!({"assignment":assignment(1)}).to_string(),
    )
    .unwrap();
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let w = worker_for(&f, model, Arc::new(FileTools), Limits::default());
    let mut next = assignment(2);
    next.resume = Some(1);
    let digest = w.execute(&next, &control());
    assert!(
        digest.contains("ended: error") && digest.contains("not frozen"),
        "{digest}"
    );
}

#[test]
fn a_legacy_start_entry_is_its_own_ancestry_root() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("legacy");
    let aja = f.path().join(".eja/ajas/1");
    std::fs::create_dir_all(&aja).unwrap();
    // A record written before view_version 1 already holds its whole starting view.
    let start = json!({"kind":"start","messages":[{"role":"system","content":"You are AJA"},{"role":"user","content":"legacy brief"}]});
    std::fs::write(aja.join("context.jsonl"), format!("{start}\n")).unwrap();
    let mut parent = assignment(1);
    parent.resume = Some(1);
    std::fs::write(
        aja.join("aja.json"),
        json!({"assignment":parent,"frozen":true}).to_string(),
    )
    .unwrap();
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let w = worker_for(&f, model.clone(), Arc::new(FileTools), Limits::default());
    let mut next = assignment(2);
    next.resume = Some(1);
    let digest = w.execute(&next, &control());
    assert!(digest.contains("ended: message"), "{digest}");
    let seen = model.seen.lock().unwrap();
    // The legacy record is where the chain stops: it is replayed once, and it does
    // not send the reader looking for an ancestor it never named.
    assert!(seen[0]
        .iter()
        .any(|message| message["content"] == "legacy brief"));
}

#[test]
fn a_record_ancestry_cycle_is_refused() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = workspace("cycle");
    for id in [1u64, 2] {
        let aja = f.path().join(format!(".eja/ajas/{id}"));
        std::fs::create_dir_all(&aja).unwrap();
        let start = json!({"kind":"start","view_version":1,"messages":[{"role":"system","content":"You are AJA"},{"role":"user","content":format!("brief {id}")}]});
        std::fs::write(aja.join("context.jsonl"), format!("{start}\n")).unwrap();
        // Each record names the other as its parent: a closed loop, not an ancestry.
        let mut parent = assignment(id);
        parent.resume = Some(if id == 1 { 2 } else { 1 });
        std::fs::write(
            aja.join("aja.json"),
            json!({"assignment":parent,"frozen":true}).to_string(),
        )
        .unwrap();
    }
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let w = worker_for(&f, model, Arc::new(FileTools), Limits::default());
    let mut next = assignment(3);
    next.resume = Some(1);
    let digest = w.execute(&next, &control());
    assert!(
        digest.contains("ended: error") && digest.contains("ancestry cycle"),
        "{digest}"
    );
}
