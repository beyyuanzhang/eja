use eja::{
    adapters::{Control, HostTools, McpTools, Model, NativeWorker, ToolContext, ToolGateway},
    aja::Limits,
    Assignment, Result, Worker,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
        let root = std::env::temp_dir().join(format!(
            "eja-mcp-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn stdio(&self, name: &str, mode: &str) -> Value {
        json!({"command":"/usr/bin/python3", "args":[script(), self.dir(name), mode], "timeout_ms":2000})
    }
    fn config(&self, value: Value) -> PathBuf {
        let path = self.0.join("mcp.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }
    fn call(
        &self,
        tools: &dyn ToolGateway,
        control: &Control,
        name: &str,
        args: Value,
    ) -> Result<Value> {
        let ctx = ToolContext {
            node_id: 123,
            work: &self.0,
            project: &self.0,
            private: &self.0,
            control,
        };
        Ok(tools.call(&ctx, name, &args)?.value)
    }
    fn events(&self, server: &str) -> Vec<Value> {
        fs::read_to_string(self.0.join(server).join("events"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn pid(&self, server: &str, name: &str) -> i32 {
        fs::read_to_string(self.0.join(server).join(name))
            .unwrap()
            .parse()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_server.py")
}
fn assignment(id: u64) -> Assignment {
    Assignment {
        task_id: id,
        objective: "use the granted connector".into(),
        issued_at: 123,
        acceptance: String::new(),
    }
}
fn control(ms: u64) -> Control {
    Control::new(Duration::from_millis(ms), Arc::new(AtomicBool::new(false)))
}
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}
#[track_caller]
fn until(mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "condition did not settle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn tool_names(tools: &dyn ToolGateway) -> Vec<String> {
    tools
        .definitions(&assignment(1))
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn stdio_discovery_grants_results_and_owned_process_group() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let mut config = f.stdio("fixture", "descendant");
    config["env"] = json!({"MCP_MARKER":"${PATH}"});
    config["tools"] = json!(["echo", "fail", "a.b"]);
    let path = f.config(json!({"mcpServers":{"fixture":config, "disabled":{"enabled":false,"command":"does-not-exist"}}}));
    let host = HostTools::new(Some(&path), &f.0, &f.0).unwrap();
    assert_eq!(
        tool_names(&host),
        [
            "read_file",
            "list_dir",
            "edit_file",
            "write_file",
            "mcp__fixture__a_b",
            "mcp__fixture__echo",
            "mcp__fixture__fail"
        ]
    );
    let pid = f.pid("fixture", "pid");
    let child = f.pid("fixture", "child_pid");
    assert!(alive(pid) && alive(child));
    let value = f
        .call(
            &host,
            &control(2000),
            "mcp__fixture__echo",
            json!({"text":"node-123"}),
        )
        .unwrap();
    assert_eq!(value["content"][0]["text"], "node-123");
    assert_eq!(value["structuredContent"]["node"], "node-123");
    assert_eq!(
        value["structuredContent"]["marker"],
        std::env::var("PATH").unwrap()
    );
    assert_eq!(value["isError"], false);
    assert_eq!(
        f.call(&host, &control(2000), "mcp__fixture__fail", json!({}))
            .unwrap()["isError"],
        true
    );
    assert!(f
        .call(&host, &control(2000), "mcp__fixture__wait", json!({}))
        .is_err());
    assert!(!f
        .events("fixture")
        .iter()
        .any(|e| e["params"]["name"] == "wait"));
    drop(host);
    until(|| !alive(pid) && !alive(child));
}

#[test]
fn cancellation_deadline_and_size_limits_leave_connection_usable() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let path = f.config(json!({"mcpServers":{"fixture":f.stdio("fixture", "stdio")}}));
    let host = McpTools::load(&path, &f.0).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let c = Control::new(Duration::from_secs(3), flag.clone());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            until(|| {
                f.events("fixture")
                    .iter()
                    .any(|e| e["method"] == "tools/call")
            });
            flag.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let error = f
            .call(&host, &c, "mcp__fixture__wait", json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2));
    });
    until(|| {
        f.events("fixture")
            .iter()
            .any(|e| e["method"] == "notifications/cancelled")
    });
    assert!(f
        .call(&host, &control(60), "mcp__fixture__wait", json!({}))
        .is_err());
    assert!(f
        .call(&host, &control(2000), "mcp__fixture__big", json!({}))
        .unwrap_err()
        .to_string()
        .contains("output exceeds"));
    assert_eq!(
        f.call(
            &host,
            &control(2000),
            "mcp__fixture__echo",
            json!({"text":"after cancel"})
        )
        .unwrap()["content"][0]["text"],
        "after cancel"
    );
    let count = f
        .events("fixture")
        .iter()
        .filter(|e| e["method"] == "tools/call")
        .count();
    assert!(f.call(&host, &c, "mcp__fixture__echo", json!({})).is_err());
    assert_eq!(
        f.events("fixture")
            .iter()
            .filter(|e| e["method"] == "tools/call")
            .count(),
        count
    );
}

#[test]
fn configuration_failure_cleans_previous_servers_and_initializing_child() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let mut hang = f.stdio("hang", "hang_init");
    hang["timeout_ms"] = json!(150);
    let path = f.config(json!({"mcpServers":{"a_good":f.stdio("good", "stdio"),"z_hang":hang}}));
    let start = Instant::now();
    assert!(McpTools::load(&path, &f.0).is_err());
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(!alive(f.pid("good", "pid")));
    assert!(!alive(f.pid("hang", "pid")));
    for bad in [
        json!({"mcpServers":{"fixture":{"command":"x","url":"https://example.com"}}}),
        json!({"mcpServers":{"fixture":{"url":"https://example.com","headers":{"Mcp-Session-Id":"x"}}}}),
        json!({"mcpServers":{"fixture":{"command":"x","env":{"TOKEN":"${EJA_MCP_UNSET_TEST_VARIABLE}"}}}}),
        json!({"mcpServers":{"fixture":{"command":"x","timeout_ms":0}}}),
        json!({"mcpServers":{"fixture":{"command":"x","typo":true}}}),
    ] {
        assert!(McpTools::load(&f.config(bad), &f.0).is_err());
    }
    for mode in ["collision", "repeat_cursor"] {
        assert!(McpTools::load(
            &f.config(json!({"mcpServers":{"fixture":f.stdio(mode,mode)}})),
            &f.0
        )
        .is_err());
        assert!(!alive(f.pid(mode, "pid")));
    }
    let mut missing = f.stdio("missing", "stdio");
    missing["tools"] = json!(["not-present"]);
    assert!(McpTools::load(&f.config(json!({"mcpServers":{"fixture":missing}})), &f.0).is_err());
    let mut empty = f.stdio("empty", "stdio");
    empty["tools"] = json!([]);
    let host = McpTools::load(&f.config(json!({"mcpServers":{"fixture":empty}})), &f.0).unwrap();
    assert!(tool_names(&host).is_empty());
}

struct HttpServer(Child);
impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn http(f: &Fixture) -> (HttpServer, String) {
    let dir = f.dir("http");
    let child = Command::new("/usr/bin/python3")
        .arg(script())
        .arg(&dir)
        .arg("http")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let guard = HttpServer(child);
    until(|| dir.join("url").exists());
    (guard, fs::read_to_string(dir.join("url")).unwrap())
}

#[test]
fn streamable_http_json_sse_headers_session_reinit_once_and_no_replay() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let (_server, url) = http(&f);
    let path = f.config(json!({"mcpServers":{"remote":{"url":url,"headers":{"Authorization":"Bearer ${USER}"},"timeout_ms":2000}}}));
    let host = McpTools::load(&path, &f.0).unwrap();
    assert_eq!(tool_names(&host).len(), 6);
    for text in ["json", "sse"] {
        assert_eq!(
            f.call(
                &host,
                &control(2000),
                "mcp__remote__echo",
                json!({"text":text})
            )
            .unwrap()["structuredContent"]["node"],
            text
        );
    }
    assert!(f
        .call(&host, &control(80), "mcp__remote__wait", json!({}))
        .is_err());
    until(|| {
        f.events("http")
            .iter()
            .any(|e| e["method"] == "notifications/cancelled")
    });
    assert_eq!(
        f.call(
            &host,
            &control(2000),
            "mcp__remote__echo",
            json!({"text":"after HTTP cancel"})
        )
        .unwrap()["content"][0]["text"],
        "after HTTP cancel"
    );
    // Even a server ignoring cancellation must not retain the client's HTTP slot.
    assert!(f
        .call(
            &host,
            &control(80),
            "mcp__remote__wait",
            json!({"text":"ignore_cancel"})
        )
        .is_err());
    assert_eq!(
        f.call(
            &host,
            &control(2000),
            "mcp__remote__echo",
            json!({"text":"after ignored cancel"})
        )
        .unwrap()["content"][0]["text"],
        "after ignored cancel"
    );
    assert!(!serde_json::to_string(&host.definitions(&assignment(1)))
        .unwrap()
        .contains("Bearer"));
    let events = f.events("http");
    let headers: Vec<_> = events
        .iter()
        .filter(|e| e["method"] == "http_headers")
        .map(|e| &e["headers"])
        .collect();
    assert!(headers
        .iter()
        .all(|h| h["authorization"] == format!("Bearer {}", std::env::var("USER").unwrap())));
    assert!(headers
        .iter()
        .any(|h| h["mcp-session-id"] == "fixture-session"
            && h["mcp-protocol-version"] == "2025-11-25"));
    assert!(f
        .call(
            &host,
            &control(2000),
            "mcp__remote__echo",
            json!({"text":"http500"})
        )
        .is_err());
    // HTTP failures must never silently repeat a potentially mutating call.
    assert_eq!(f.events("http").iter().filter(|e| e["method"] == "tools/call" && e["params"]["arguments"]["text"] == "http500").count(),1);
    drop(host);
    until(|| {
        f.events("http")
            .iter()
            .any(|e| e["method"] == "http_delete")
    });
    let host = McpTools::load(&path, &f.0).unwrap();
    let inits = f
        .events("http")
        .iter()
        .filter(|e| e["method"] == "initialize")
        .count();
    assert!(f
        .call(
            &host,
            &control(2000),
            "mcp__remote__echo",
            json!({"text":"expired"})
        )
        .is_err());
    // An expired session means the request did not execute: reinitialize once and
    // retry that request exactly once.
    assert_eq!(
        f.events("http")
            .iter()
            .filter(|e| e["method"] == "initialize")
            .count(),
        inits + 1
    );
    assert_eq!(f.events("http").iter().filter(|e| e["method"] == "tools/call" && e["params"]["arguments"]["text"] == "expired").count(),2);
}

struct ConnectorModel;
impl Model for ConnectorModel {
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        _: &Limits,
        _: &Control,
        _json_mode: bool,
    ) -> Result<Value> {
        assert!(tools
            .iter()
            .any(|t| t["function"]["name"] == "mcp__fixture__echo"));
        if messages.last().unwrap()["role"] == "tool" {
            let result: Value =
                serde_json::from_str(messages.last().unwrap()["content"].as_str().unwrap())?;
            assert_eq!(result["structuredContent"]["node"], "native node");
            Ok(
                json!({"role":"assistant","content":json!({"status":"done","summary":"MCP used","artifacts":[],"evidence":[],"open_questions":[]}).to_string()}),
            )
        } else {
            Ok(
                json!({"role":"assistant","content":null,"tool_calls":[{"id":"mcp-call","type":"function","function":{"name":"mcp__fixture__echo","arguments":"{\"text\":\"native node\"}"}}]}),
            )
        }
    }
}

#[test]
fn native_nodes_share_one_host_connection_and_freeze_independently() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let path = f.config(json!({"mcpServers":{"fixture":f.stdio("fixture", "stdio")}}));
    let tools = Arc::new(HostTools::new(Some(&path), &f.0, &f.0).unwrap());
    let pid = f.pid("fixture", "pid");
    let root = f.0.join("nodes");
    let worker = NativeWorker::new(
        env!("CARGO_BIN_EXE_eja"),
        &root,
        &f.0,
        Arc::new(ConnectorModel),
        tools.clone(),
    )
    .unwrap();
    let fd_dir = if Path::new("/dev/fd").is_dir() {
        "/dev/fd"
    } else {
        "/proc/self/fd"
    };
    let fds = fs::read_dir(fd_dir).unwrap().count();
    for id in 1..=12 {
        let digest = worker.execute(&assignment(id));
        assert!(digest.contains("ended: message"), "{digest}");
        assert_eq!(worker.active_count(), 0);
        assert!(!root.join(format!("{id}/work")).exists());
        let manifest: Value =
            serde_json::from_slice(&fs::read(root.join(format!("{id}/node.json"))).unwrap())
                .unwrap();
        assert_eq!(manifest["frozen"], true);
        assert!(alive(pid));
    }
    assert_eq!(
        fs::read_dir(fd_dir).unwrap().count(),
        fds,
        "MCP nodes leaked file descriptors"
    );
    assert_eq!(
        f.events("fixture")
            .iter()
            .filter(|e| e["method"] == "initialize")
            .count(),
        1
    );
    drop(worker);
    assert!(alive(pid));
    drop(tools);
    assert!(!alive(pid));
}

#[test]
fn cli_catalog_uses_owned_default_config_and_fails_explicit_missing_path() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let path = f.config(json!({"mcpServers":{"fixture":f.stdio("fixture", "stdio")}}));
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .args(["--dir", f.0.to_str().unwrap(), "show", "tools"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "mcp__fixture__echo"));
    assert!(!alive(f.pid("fixture", "pid")));
    fs::remove_file(path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_eja"))
        .args([
            "--dir",
            f.0.to_str().unwrap(),
            "--mcp-config",
            f.0.join("missing").to_str().unwrap(),
            "tools",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn host_declared_read_only_tools_are_observable() {
    let _lock = LOCK.lock().unwrap();
    let f = Fixture::new();
    let mut config = f.stdio("fixture", "descendant");
    config["read_only"] = json!(["echo"]);
    let path = f.config(json!({"mcpServers":{"fixture":config}}));
    let host = HostTools::new(Some(&path), &f.0, &f.0).unwrap();
    let value =
        eja::adapters::Observer::observe(&host, "mcp__fixture__echo", &json!({"text":"probe"}))
            .expect("declared read-only tool is observable");
    assert!(value.to_string().contains("probe") || !value.is_null());
    assert!(eja::adapters::Observer::observe(&host, "mcp__fixture__fail", &json!({})).is_err());
}
