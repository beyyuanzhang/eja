use eja::adapters::{Observer, Ports};
use eja::{
    turn, Action, Brain, Decision, EjaContext, Kind, LookSource, Result, Selector, Store, Worker,
};
use std::collections::VecDeque;
use std::sync::Mutex;

struct Script(Mutex<VecDeque<Decision>>);

impl Brain for Script {
    fn decide(&self, _: &EjaContext) -> Decision {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted decision")
    }
}

struct EmptyWorker;

impl Worker for EmptyWorker {
    fn execute(&self, _: &eja::Assignment) -> String {
        format!(
            "0 steps, 0 tool calls, 0 artifacts, ended: error\n{}",
            "no work expected"
        )
    }
}

struct Weather;

impl Observer for Weather {
    fn observe(&self, tool: &str, arguments: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"tool": tool, "echo": arguments, "sky": "clear"}))
    }
}

#[test]
fn a_world_look_with_nothing_waiting_is_refused() {
    let dir = std::env::temp_dir().join(format!("eja-observe-nudge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let brain = Script(Mutex::new(VecDeque::from(vec![
        Decision {
            reply: String::new(),
            actions: vec![Action::Look {
                source: LookSource::World,
                target: "mcp__weather__get".to_string(),
                arguments: serde_json::json!({"city": "berlin"}),
                selector: Selector::Whole,
            }],
            refused: Vec::new(),
        },
        Decision {
            reply: "then do the work".into(),
            actions: vec![],
            refused: Vec::new(),
        },
    ])));
    let out = turn(
        &store,
        "what is the sky?",
        &brain,
        &EmptyWorker,
        &Ports {
            world: Some(&Weather),
            ..Default::default()
        },
    )
    .expect("turn");
    assert!(out.evidence.is_empty());
    assert_eq!(out.refused.len(), 1);
    assert!(out.refused[0].contains("waiting task"));
    assert!(store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::Refused));
}

#[test]
fn observing_a_read_only_tool_records_a_view() {
    let dir = std::env::temp_dir().join(format!("eja-observe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let brain = Script(Mutex::new(VecDeque::from(vec![
        // The world is read while judging, so the task is opened and run first.
        Decision {
            reply: String::new(),
            actions: vec![Action::OpenTask {
                title: "sky".into(),
                request: "check the sky".into(),
                acceptance: String::new(),
                run: true,
            }],
            refused: Vec::new(),
        },
        Decision {
            reply: String::new(),
            actions: vec![Action::Look {
                source: LookSource::World,
                target: "mcp__weather__get".to_string(),
                arguments: serde_json::json!({"city": "berlin"}),
                selector: Selector::Whole,
            }],
            refused: Vec::new(),
        },
        Decision {
            reply: "it is clear".into(),
            actions: vec![],
            refused: Vec::new(),
        },
    ])));
    let out = turn(
        &store,
        "what is the sky?",
        &brain,
        &EmptyWorker,
        &Ports {
            world: Some(&Weather),
            ..Default::default()
        },
    )
    .expect("turn");
    assert_eq!(out.reply, "it is clear");
    assert_eq!(out.context.evidence.len(), 1);
    assert_eq!(out.context.evidence[0].source, LookSource::World);
    assert_eq!(out.context.evidence[0].result["sky"], "clear");
    assert!(store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::Observed));
    let rendered = eja::adapters::render_context(&out.context);
    assert!(
        rendered.contains("EVIDENCE (1):"),
        "{}",
        &rendered[..200.min(rendered.len())]
    );
    assert!(rendered.contains("clear"));
}

#[test]
fn eja_can_observe_project_files_but_never_write() {
    use eja::adapters::HostTools;
    let root = std::env::temp_dir().join(format!("eja-observe-fs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let private = root.join("state");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&private).expect("private");
    std::fs::write(project.join("note.txt"), "hello from the project").expect("write");
    let host = HostTools::new(None, &project, &private).expect("tools");
    let value = Observer::observe(&host, "read_file", &serde_json::json!({"path":"note.txt"}))
        .expect("project file is observable");
    assert!(value.to_string().contains("hello"));
    // There is no second area to read from any more; the world is one place.
    assert!(Observer::observe(&host, "list_dir", &serde_json::json!({"path": ".."})).is_err());
    assert!(Observer::observe(
        &host,
        "write_file",
        &serde_json::json!({"path":"x.txt","text":"y"})
    )
    .is_err());
    assert!(Observer::observe(
        &host,
        "read_file",
        &serde_json::json!({"path":"../state/secret"})
    )
    .is_err());
    // An edit is an effect: the observer names the refusal instead of pretending
    // the tool is a connector.
    let edited = Observer::observe(
        &host,
        "edit_file",
        &serde_json::json!({"path":"note.txt","find":"hello","replace":"bye"}),
    );
    assert!(edited
        .unwrap_err()
        .to_string()
        .contains("edits belong to nodes"));
}
