use eja::{
    turn_observed, Action, Brain, Decision, EjaContext, Kind, NoopArtifactReader, NoopRetriever,
    Observer, Report, Result, Store, Worker,
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
    fn execute(&self, _: &eja::Assignment) -> Report {
        Report::failed("no work expected")
    }
}

struct Weather;

impl Observer for Weather {
    fn observe(&self, tool: &str, arguments: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"tool": tool, "echo": arguments, "sky": "clear"}))
    }
}

#[test]
fn observing_a_read_only_tool_records_a_view() {
    let dir = std::env::temp_dir().join(format!("eja-observe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let brain = Script(Mutex::new(VecDeque::from(vec![
        Decision {
            reply: String::new(),
            actions: vec![Action::Observe {
                tool: "mcp__weather__get".into(),
                arguments: serde_json::json!({"city": "berlin"}),
            }],
        },
        Decision {
            reply: "it is clear".into(),
            actions: vec![],
        },
    ])));
    let out = turn_observed(
        &store,
        "what is the sky?",
        &brain,
        &EmptyWorker,
        &NoopRetriever,
        &NoopArtifactReader,
        &Weather,
    )
    .expect("turn");
    assert_eq!(out.reply, "it is clear");
    assert_eq!(out.context.observations.len(), 1);
    assert_eq!(out.context.observations[0].tool, "mcp__weather__get");
    assert_eq!(out.context.observations[0].result["sky"], "clear");
    assert!(store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::Observed));
    let rendered = eja::adapters::render_context(&out.context);
    let at = rendered.find("OBSERV").unwrap_or(0);
    assert!(
        rendered.contains("OBSERVATIONS (1):"),
        "{}",
        &rendered[at..(at + 200).min(rendered.len())]
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
    let value = Observer::observe(
        &host,
        "read_file",
        &serde_json::json!({"area":"project","path":"note.txt"}),
    )
    .expect("project file is observable");
    assert!(value.to_string().contains("hello"));
    assert!(Observer::observe(
        &host,
        "read_file",
        &serde_json::json!({"area":"work","path":"note.txt"})
    )
    .is_err());
    assert!(Observer::observe(
        &host,
        "write_file",
        &serde_json::json!({"path":"x.txt","text":"y"})
    )
    .is_err());
    assert!(Observer::observe(
        &host,
        "read_file",
        &serde_json::json!({"area":"project","path":"../state/secret"})
    )
    .is_err());
}
