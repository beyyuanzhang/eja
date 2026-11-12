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
