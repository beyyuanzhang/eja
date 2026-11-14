use eja::{
    adapters::{
        Control, FileTools, Model, NativeWorker, OpenAiModel, ToolContext, ToolGateway, ToolOutput,
    },
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
fn calls(items: Vec<Value>) -> Value {
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
    let digest = w.execute(&assignment(1));
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
    assert!(w.execute(&assignment(1)).contains("ended: error"));
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
    fn call(&self, c: &ToolContext<'_>, _: &str, args: &Value) -> Result<ToolOutput> {
        let path = args["path"].as_str().unwrap_or_default();
        match args["kind"].as_str().unwrap_or_default() {
            "project" => {
                let target = c.project.join(path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, "delivered")?;
                Ok(ToolOutput {
                    value: json!({"ok": true}),
                })
            }
            "drop" => {
                std::fs::remove_file(c.project.join(path))?;
                Ok(ToolOutput {
                    value: json!({"ok": true}),
                })
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
    let digest = w.execute(&assignment(1));
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
    let digest = w.execute(&assignment(1));
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
    fn call(&self, _: &ToolContext<'_>, _: &str, _: &Value) -> Result<ToolOutput> {
        Ok(ToolOutput {
            value: json!({"text": "x".repeat(64 * 1024)}),
        })
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
    w.execute(&assignment(1));
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
    w.execute(&assignment(1));
    let mut second = assignment(2);
    second.resume = Some(1);
    w.execute(&second);
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
    fn call(&self, c: &ToolContext<'_>, name: &str, args: &Value) -> Result<ToolOutput> {
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
    assert!(w.execute(&assignment(1)).contains("ended: message"));
    assert!(w.execute(&assignment(2)).contains("ended: message"));
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
    let digest = w.execute(&assignment(1));
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
    let r = w.execute(&assignment(1));
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
    let r = w.execute(&assignment(2));
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
    let r = w.execute(&assignment(3));
    assert!(r.contains("ended: error"), "{r}");
    assert!(r.contains("output limit"), "{r}");
    assert_eq!(w.active_count(), 0);
    assert_eq!(f.manifest(3)["frozen"], true);
    // A plain-text final message is a claim, not a schema breach.
    let plain = Arc::new(Script::new(vec![
        json!({"role":"assistant","content":"not a digest"}),
    ]));
    let w = f.worker(plain.clone(), Arc::new(FileTools), Limits::default());
    let r = w.execute(&assignment(5));
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
        let join = std::thread::spawn(move || running.execute(&assignment(1)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let pids = worker_pids();
        assert!(!pids.is_empty());
        assert!(w.execute(&assignment(2)).contains("ended: budget"));
        assert!(!f.0.join(".eja/nodes/2").exists());
        if cancel {
            assert!(w.cancel(1));
        }
        let r = join.join().unwrap();
        assert!(r.contains("ended: error"), "{r}");
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
        assert!(w.execute(&assignment(id)).contains("ended: message"));
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
        json!({"assignment":assignment(1),"limits":Limits::default()})
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
    let digest = w.execute(&assignment(1));
    assert!(digest.contains("ended: message"), "{digest}");
    let seen = server.join().unwrap();
    assert_eq!(seen[0]["messages"][1]["content"], "node 1 work");
    assert_eq!(seen[0]["tools"].as_array().unwrap().len(), 4);
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
        json!({"choices":[{"finish_reason":"tool_calls","message":calls(vec![call("read_file",json!({})),call("read_file",json!({}))])}]}),
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
    let digest = worker.execute(&input);
    assert!(digest.contains("ended: error"), "{digest}");
    assert!(digest.contains("frame exceeds limit"), "{digest}");
    assert_eq!(worker.active_count(), 0);
    assert!(worker_pids().is_empty());
    assert_eq!(f.manifest(1)["frozen"], true);
}

#[test]
fn cli_eja_dispatches_native_node_reads_frozen_output_and_accepts() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
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
                "write_file",
                json!({"path":"checklist.txt","text":"build\ntest"}),
            )]),
        ),
        response("stop", final_message(&["checklist.txt"])),
        response(
            "stop",
            decision(
                "reading",
                json!([{"op":"look","source":"node","target":"1/artifacts/checklist.txt"}]),
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
    assert!(!log.contains("test-key"));
    assert!(worker_pids().is_empty());
}
