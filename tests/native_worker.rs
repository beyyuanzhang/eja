mod support;
use eja::{
    adapters::{Control, FileTools, Model, NativeWorker, OpenAiModel, ToolContext, ToolGateway},
    aja::Limits,
    Assignment, Result, Worker,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use support::control;

static SERIAL: AtomicU64 = AtomicU64::new(0);
static LOCK: Mutex<()> = Mutex::new(());

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "eja-native-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
    fn worker(
        &self,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolGateway>,
        limits: Limits,
    ) -> Arc<NativeWorker> {
        Arc::new(
            NativeWorker::new(
                env!("CARGO_BIN_EXE_eja"),
                self.0.join(".eja/nodes"),
                &self.0,
                model,
                tools,
            )
            .unwrap()
            .with_limits(limits)
            .unwrap(),
        )
    }
    fn manifest(&self, id: u64) -> Value {
        serde_json::from_slice(
            &std::fs::read(self.0.join(format!(".eja/nodes/{id}/node.json"))).unwrap(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assignment(id: u64) -> Assignment {
    Assignment {
        task_id: id,
        objective: format!("node {id} work"),
        issued_at: 123,
        acceptance: String::new(),
        input: String::new(),
        resume: None,
    }
}
fn final_message(artifacts: &[&str]) -> Value {
    json!({"role":"assistant","content":json!({"status":"done","summary":"finished","artifacts":artifacts,"evidence":[],"open_questions":[]}).to_string()})
}
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call("write_file", json!({"path":"out.txt","text":"node result"})),
            call("read_file", json!({"path":"out.txt"})),
        ]),
        final_message(&["out.txt"]),
    ]));
    let w = f.worker(model.clone(), Arc::new(FileTools), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert!(digest.contains("project: +1 created (out.txt)"), "{digest}");
    assert_eq!(
        std::fs::read_to_string(f.0.join("out.txt")).unwrap(),
        "node result",
        "the node writes the workspace itself"
    );
    assert!(f.manifest(1)["checkpoint"].as_str().is_some());
    assert_eq!(f.manifest(1)["frozen"], true);
    assert_eq!(f.manifest(1)["context"], "context.jsonl");
    assert!(
        std::fs::metadata(f.0.join(".eja/nodes/1/node.json"))
            .unwrap()
            .permissions()
            .readonly(),
        "the frozen manifest is read-only"
    );
    let context = std::fs::read_to_string(f.0.join(".eja/nodes/1/context.jsonl")).unwrap();
    for kind in ["start", "assistant", "tool", "finish"] {
        assert!(
            context.contains(&format!("\"kind\":\"{kind}\"")),
            "context is missing {kind}: {context}"
        );
    }
    assert!(context.contains("node result"));
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen[0][1]["content"], "node 1 work");
    assert_eq!(seen[1][3]["role"], "tool");
    let read_result = seen[1]
        .iter()
        .find(|m| m["tool_call_id"] == "call_read_file")
        .unwrap();
    assert!(read_result["content"]
        .as_str()
        .unwrap()
        .contains("node result"));
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
    let before = f.manifest(1);
    assert!(w
        .execute(&assignment(1), &control())
        .contains("ended: error"));
    assert_eq!(f.manifest(1), before);
}

/// Effects that reach past the work area, so the digest can be checked against
/// the world rather than the node's own account.
struct Effects;
impl ToolGateway for Effects {
    fn definitions(&self, _: &Assignment) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"effect","description":"apply one effect","parameters":{"type":"object","properties":{"kind":{"type":"string"},"path":{"type":"string"}},"required":["kind","path"]}}}),
        ]
    }
    fn call(&self, c: &ToolContext<'_>, _: &str, args: &Value) -> Result<Value> {
        let path = args["path"].as_str().unwrap_or_default();
        match args["kind"].as_str().unwrap_or_default() {
            "project" => {
                let target = c.project.join(path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, "delivered")?;
                Ok(json!({"ok": true}))
            }
            "drop" => {
                std::fs::remove_file(c.project.join(path))?;
                Ok(json!({"ok": true}))
            }
            other => Err(format!("unknown effect {other}").into()),
        }
    }
}

#[test]
fn the_digest_reports_what_the_project_gained() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call("effect", json!({"kind":"project","path":"delivered.txt"})),
            call("effect", json!({"kind":"project","path":"nested/also.txt"})),
        ]),
        final_message(&[]),
    ]));
    let w = f.worker(model, Arc::new(Effects), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(
        digest.contains("project: +2 created (delivered.txt, nested/also.txt)"),
        "{digest}"
    );
}

#[test]
fn the_node_leaves_a_fossil_of_the_world_it_ended_in() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "effect",
            json!({"kind":"project","path":"report.md"}),
        )]),
        final_message(&[]),
    ]));
    let w = f.worker(model, Arc::new(Effects), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("+1 created"), "{digest}");
    let manifest = f.manifest(1);
    let commit = manifest["checkpoint"]
        .as_str()
        .expect("a node records its checkpoint");
    assert_eq!(commit.len(), 40, "{commit}");
    let read = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(f.0.join(".eja/checkpoints"))
        .arg("show")
        .arg(format!("{commit}:report.md"))
        .output()
        .expect("git");
    assert_eq!(String::from_utf8_lossy(&read.stdout), "delivered");
}

/// A gateway that answers with something far larger than a node should carry.
struct Big;
impl ToolGateway for Big {
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        calls(vec![call("big", json!({}))]),
        final_message(&[]),
    ]));
    let w = f.worker(model.clone(), Arc::new(Big), Limits::default());
    w.execute(&assignment(1), &control());
    let record = std::fs::read_to_string(f.0.join(".eja/nodes/1/context.jsonl")).unwrap();
    assert!(
        record.contains(&"x".repeat(60 * 1024)),
        "the record keeps it whole"
    );
    let seen = model.seen.lock().unwrap();
    let carried = seen[1]
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the node saw a tool result");
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
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
    let w = f.worker(model.clone(), Arc::new(Effects), Limits::default());
    w.execute(&assignment(1), &control());
    let mut second = assignment(2);
    second.resume = Some(1);
    w.execute(&second, &control());
    let seen = model.seen.lock().unwrap();
    let first_call_of_second_node = &seen[2];
    assert!(
        first_call_of_second_node[0]["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("You are AJA")),
        "the predecessor transcript is inherited"
    );
    assert!(
        first_call_of_second_node
            .iter()
            .any(|message| message["role"] == "tool"),
        "the inherited turn carries its tool result"
    );
    let brief = first_call_of_second_node
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default();
    assert!(brief.starts_with("Continuing as a new node."), "{brief}");
}

struct Policy;
impl ToolGateway for Policy {
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
fn nodes_share_the_workspace_and_grants_still_differ() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
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
    let w = f.worker(model.clone(), Arc::new(Policy), Limits::default());
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
    // There is one workspace: the second node reads what the first wrote.
    assert!(results[0].contains("one"), "{}", results[0]);
    // The grant, not the filesystem, is what a node may not cross.
    assert!(results[1].contains("not allowed"), "{}", results[1]);
    assert_eq!(
        std::fs::read_to_string(f.0.join("same.txt")).unwrap(),
        "one"
    );
}

#[test]
fn traversal_private_state_and_symlinks_are_not_file_capabilities() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    std::fs::create_dir_all(f.0.join(".eja")).unwrap();
    std::fs::write(f.0.join(".eja/secret.txt"), "private").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(f.0.join(".eja/secret.txt"), f.0.join("alias.txt")).unwrap();
    let model = Arc::new(Script::new(vec![
        calls(vec![
            call("write_file", json!({"path":"../escape.txt","text":"bad"})),
            call("read_file", json!({"path":".eja/secret.txt"})),
            call("read_file", json!({"path":"alias.txt"})),
        ]),
        final_message(&["../escape.txt"]),
    ]));
    let w = f.worker(model.clone(), Arc::new(FileTools), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    // The claim mentions an escaping path; the world did not move.
    assert!(digest.contains("project: unchanged"), "{digest}");
    assert!(!f.0.join("escape.txt").exists());
    assert_eq!(f.manifest(1)["frozen"], true);
    assert_eq!(
        model.seen.lock().unwrap()[1]
            .iter()
            .filter(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("error"))
            .count(),
        3
    );
}

#[test]
fn step_context_and_output_limits_stop_the_node_and_preserve_partial_work() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![calls(vec![call(
        "write_file",
        json!({"path":"partial.txt","text":"kept"}),
    )])]));
    let w = f.worker(
        model.clone(),
        Arc::new(FileTools),
        Limits {
            steps: Some(1),
            ..Limits::default()
        },
    );
    let r = w.execute(&assignment(1), &control());
    // A spent budget stops the node without calling its work a failure.
    assert!(r.contains("ended: budget"), "{r}");
    assert!(r.contains("budget"), "{r}");
    assert!(r.contains("project: +1 created (partial.txt)"), "{r}");
    assert!(f.manifest(1)["digest"]
        .as_str()
        .unwrap()
        .contains("ended: budget"));
    let context_limits = Limits {
        context_bytes: 16,
        ..Limits::default()
    };
    let context_model = Arc::new(Script::new(vec![]));
    let w = f.worker(context_model, Arc::new(FileTools), context_limits);
    let r = w.execute(&assignment(2), &control());
    assert!(r.contains("ended: budget"), "{r}");
    assert!(r.contains("context budget"), "{r}");
    assert_eq!(w.active_count(), 0);
    assert_eq!(f.manifest(2)["frozen"], true);
    // A protocol breach is an error, not a budget the node spent.
    let oversized = Arc::new(Script::new(vec![json!({
        "role": "assistant",
        "content": "x".repeat(129),
    })]));
    let w = f.worker(
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
    assert_eq!(f.manifest(3)["frozen"], true);
    // A plain-text final message is a claim, not a schema breach.
    let plain = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":"not a digest"}),
    ]));
    let w = f.worker(plain.clone(), Arc::new(FileTools), Limits::default());
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
fn timeout_cancellation_and_busy_slot_release_all_node_resources() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for cancel in [false, true] {
        let f = Fixture::new();
        let started = Arc::new(AtomicBool::new(false));
        let w = f.worker(
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
        assert!(w
            .execute(&assignment(2), &control())
            .contains("ended: budget"));
        assert!(!f.0.join(".eja/nodes/2").exists());
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
        assert_eq!(f.manifest(1)["frozen"], true);
        assert_reaped(&pids);
    }
}

#[test]
fn a_node_stopped_mid_run_claims_no_invented_project_change() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Cancelled while the model is working.
    let f = Fixture::new();
    std::fs::write(f.0.join("kept.txt"), "work").unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let w = f.worker(
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
    // The run ended before the world could be read again: an incomplete
    // inventory is not evidence that the project lost its files.
    assert!(!digest.contains("deleted"), "{digest}");
    assert!(!digest.contains("created"), "{digest}");
    assert!(digest.contains("inventory truncated"), "{digest}");
    // A turn deadline that expires before the node could do anything.
    let g = Fixture::new();
    std::fs::write(g.0.join("kept.txt"), "work").unwrap();
    let w = g.worker(
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
fn many_frozen_nodes_have_no_live_workers_or_growing_pipe_handles() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new((0..25).map(|_| final_message(&[])).collect()));
    let w = f.worker(model.clone(), Arc::new(FileTools), Limits::default());
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
        std::fs::read_dir(f.0.join(".eja/nodes")).unwrap().count(),
        25
    );
}

#[test]
fn native_worker_exits_when_its_host_pipe_disappears() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut child = Command::new(env!("CARGO_BIN_EXE_eja"))
        .arg("--aja-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let (url, server) = serve(vec![
        json!({"choices":[{"finish_reason":"tool_calls","message":calls(vec![call("write_file",json!({"path":"http.txt","text":"real host round trip"}))])}]}),
        json!({"choices":[{"finish_reason":"stop","message":final_message(&["http.txt"])}]}),
    ]);
    let w = f.worker(
        Arc::new(OpenAiModel::new(url, "test-key", "test-model")),
        Arc::new(FileTools),
        Limits::default(),
    );
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"), "{digest}");
    let seen = server.join().unwrap();
    assert_eq!(seen[0]["messages"][1]["content"], "node 1 work");
    assert_eq!(seen[0]["tools"].as_array().unwrap().len(), 5);
    assert!(seen[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool"));
    assert!(
        digest.contains("project: +1 created (http.txt)"),
        "{digest}"
    );
}

#[test]
fn transport_rejects_truncation_duplicate_calls_and_oversized_responses() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::default());
    let worker = f.worker(model, Arc::new(FileTools), Limits::default());
    let mut input = assignment(1);
    input.objective = "x".repeat(eja::aja::FRAME_LIMIT);
    let digest = worker.execute(&input, &control());
    assert!(digest.contains("ended: budget"), "{digest}");
    assert!(digest.contains("context budget"), "{digest}");
    assert_eq!(worker.active_count(), 0);
    assert!(worker_pids().is_empty());
    assert_eq!(f.manifest(1)["frozen"], true);
}

#[test]
fn cli_eja_dispatches_native_node_reads_frozen_output_and_accepts() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let fixture = f.0.join(".eja/fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(f.0.join(".eja/mcp.json"), json!({"mcpServers":{"files":{
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
                json!([{"op":"look","source":"node","target":"1/checklist.txt"}]),
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
        .current_dir(&f.0)
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
    assert_eq!(f.manifest(1)["frozen"], true);
    let log = std::fs::read_to_string(f.0.join(".eja/log.jsonl")).unwrap();
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
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
    let w = f.worker(model.clone(), Arc::new(Big), Limits::default());
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
    let record = std::fs::read_to_string(f.0.join(".eja/nodes/2/context.jsonl")).unwrap();
    assert!(
        !record.contains(&"x".repeat(1000)),
        "a continuation stores a parent reference, not a copied payload"
    );
    assert!(record.len() < 5000, "{}", record.len());
}

struct Passage;
impl ToolGateway for Passage {
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
                    json!({"node":view["record"]["node"],"entry":view["record"]["entry"],"offset":35980,"bytes":128}),
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let w = f.worker(
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
        adapters::{ArtifactReader, FileArtifactReader},
        Selector,
    };
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        calls(vec![call(
            "write_file",
            json!({"path":"out.txt","text":"第一行\n第二行\n第三行\n"}),
        )]),
        final_message(&[]),
    ]));
    let w = f.worker(model, Arc::new(FileTools), Limits::default());
    assert!(w
        .execute(&assignment(1), &control())
        .contains("ended: message"));
    let reader =
        FileArtifactReader::with_checkpoints(f.0.join(".eja/nodes"), f.0.join(".eja/checkpoints"));
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    // The host records the assistant call before an over-sized model response
    // can be sent over IPC. Simulate a frozen, interrupted record explicitly.
    let node = f.0.join(".eja/nodes/1");
    std::fs::create_dir_all(&node).unwrap();
    let start = json!({"kind":"start","view_version":1,"messages":[{"role":"system","content":"You are AJA"},{"role":"user","content":"old brief"}]});
    let completed =
        json!({"kind":"assistant","message":{"role":"assistant","content":"completed-prefix"}});
    let unfinished = json!({"kind":"assistant","message":calls(vec![call("big",json!({}))])});
    std::fs::write(
        node.join("context.jsonl"),
        format!("{start}\n{completed}\n{unfinished}\n"),
    )
    .unwrap();
    std::fs::write(
        node.join("node.json"),
        json!({"assignment":assignment(1),"frozen":true}).to_string(),
    )
    .unwrap();
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let w = f.worker(model.clone(), Arc::new(Big), Limits::default());
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
fn a_turn_deadline_cannot_be_extended_by_a_nodes_own_timeout() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let started = Arc::new(AtomicBool::new(false));
    let w = f.worker(
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
    assert_eq!(f.manifest(1)["frozen"], true);
    assert!(f.manifest(1)["checkpoint_error"]
        .as_str()
        .unwrap()
        .contains("timed out"));
}

#[test]
fn record_addresses_cannot_expand_a_nodes_ancestry_grant() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        calls(vec![call("eja_read_record", json!({"node":999,"entry":0}))]),
        final_message(&[]),
    ]));
    let w = f.worker(model.clone(), Arc::new(Big), Limits::default());
    let digest = w.execute(&assignment(1), &control());
    assert!(digest.contains("ended: message"));
    let seen = model.seen.lock().unwrap();
    assert!(seen[1].iter().any(|message| message["role"] == "tool"
        && message["content"]
            .as_str()
            .unwrap()
            .contains("outside this node's ancestry")));
}

#[test]
fn relative_private_root_is_canonical_and_excluded_from_project_evidence() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    std::fs::create_dir_all(f.0.join(".eja")).unwrap();
    std::fs::write(f.0.join(".eja/secret"), "private-token").unwrap();
    std::fs::write(f.0.join("public.txt"), "public").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(f.0.strip_prefix("/").unwrap());
    relative.push(".eja/nodes");
    assert!(relative.is_relative());
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let worker = NativeWorker::new(
        env!("CARGO_BIN_EXE_eja"),
        relative,
        &f.0,
        model,
        Arc::new(FileTools),
    )
    .unwrap();
    let digest = worker.execute(&assignment(1), &control());
    assert!(digest.contains("project: unchanged"), "{digest}");
    let manifest = f.manifest(1);
    let commit = manifest["checkpoint"].as_str().unwrap();
    let tree = Command::new("git")
        .arg("--git-dir")
        .arg(f.0.join(".eja/checkpoints"))
        .args(["ls-tree", "-r", "--name-only", commit])
        .output()
        .unwrap();
    assert!(tree.status.success());
    assert_eq!(String::from_utf8(tree.stdout).unwrap().trim(), "public.txt");
}

#[test]
fn a_valid_long_assignment_crosses_start_ipc_once() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![final_message(&[])]));
    let worker = f.worker(model.clone(), Arc::new(FileTools), Limits::default());
    let mut input = assignment(1);
    input.objective = "x".repeat(200 * 1024);
    let digest = worker.execute(&input, &control());
    assert!(digest.contains("ended: message"), "{digest}");
    assert_eq!(model.seen.lock().unwrap()[0][1]["content"], input.objective);
}

#[test]
fn malformed_custom_model_messages_are_recorded_as_errors_and_not_replayed() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":null}),
        final_message(&[]),
    ]));
    let worker = f.worker(model.clone(), Arc::new(FileTools), Limits::default());
    let first = worker.execute(&assignment(1), &control());
    assert!(first.contains("ended: error"), "{first}");
    let record = std::fs::read_to_string(f.0.join(".eja/nodes/1/context.jsonl")).unwrap();
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
    let f = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .args(["--dir", f.0.to_str().unwrap(), "show", "turn"])
        .env("LNA_BASE_URL", "http://127.0.0.1:1")
        .env("LNA_API_KEY", "test")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown show target"));
    assert!(!f.0.join("log.jsonl").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .current_dir(&f.0)
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
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let model = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":"completed prefix","tool_calls":[]}),
        final_message(&[]),
    ]));
    let worker = f.worker(model.clone(), Arc::new(FileTools), Limits::default());
    assert!(worker
        .execute(&assignment(1), &control())
        .contains("ended: message"));
    let mut next = assignment(2);
    next.resume = Some(1);
    assert!(worker.execute(&next, &control()).contains("ended: message"));
    assert!(model.seen.lock().unwrap()[1]
        .iter()
        .any(|m| m["content"] == "completed prefix"));
    let damaged = f.0.join(".eja/nodes/3");
    std::fs::create_dir_all(&damaged).unwrap();
    std::fs::write(
        damaged.join("node.json"),
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
