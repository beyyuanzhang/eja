mod support;

use eja::adapters::{Capabilities, Ports};
use eja::{Action, Decision, Kind, Result, Selector};
use support::{control, decision, Done, Script, State};

/// A decision whose reply the turn states. The kit's `decision` always says
/// "structure"; this file asserts what the turn answered.
fn say(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.to_string(),
        actions,
        refused: Vec::new(),
    }
}

struct Weather;

impl Capabilities for Weather {
    fn observe(
        &self,
        tool: &str,
        arguments: &serde_json::Value,
        _: &eja::Control,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"tool": tool, "echo": arguments, "sky": "clear"}))
    }
}

#[test]
fn a_world_look_does_not_depend_on_waiting_tasks() {
    let fixture = State::new("observe-nudge");
    let store = fixture.store();
    let brain = Script::new(vec![
        decision(vec![Action::Look {
            target: "mcp__weather__get".to_string(),
            arguments: serde_json::json!({"city": "berlin"}),
            selector: Selector::Whole,
        }]),
        decision(vec![]),
    ]);
    let out = eja::turn(
        &store,
        "what is the sky?",
        &brain,
        &Done::default(),
        &Ports {
            capabilities: Some(&Weather),
            ..Default::default()
        },
    )
    .expect("turn");
    assert_eq!(out.context().evidence.len(), 1);
    assert!(out.context().refused.is_empty());
    assert!(!store
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == Kind::Refused));
}

#[test]
fn observing_a_read_only_tool_records_a_view() {
    let fixture = State::new("observe");
    let store = fixture.store();
    let brain = Script::new(vec![
        // The world is read while judging, so the task is opened and run first.
        decision(vec![Action::OpenTask {
            title: "sky".into(),
            request: "check the sky".into(),
            acceptance: String::new(),
            evidence: None,
            run: true,
        }]),
        decision(vec![Action::Look {
            target: "mcp__weather__get".to_string(),
            arguments: serde_json::json!({"city": "berlin"}),
            selector: Selector::Whole,
        }]),
        say("it is clear", vec![]),
    ]);
    let out = eja::turn(
        &store,
        "what is the sky?",
        &brain,
        &Done::default(),
        &Ports {
            capabilities: Some(&Weather),
            ..Default::default()
        },
    )
    .expect("turn");
    assert_eq!(out.reply, "it is clear");
    assert_eq!(out.context().evidence.len(), 1);
    assert_eq!(out.context().evidence[0].target, "mcp__weather__get");
    assert_eq!(out.context().evidence[0].result["sky"], "clear");
    assert!(store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::Observed));
    let rendered = eja::adapters::render_context(&out.context());
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
    let fixture = State::new("observe-fs");
    let project = fixture.path().join("project");
    let private = fixture.path().join("state");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&private).expect("private");
    std::fs::write(project.join("note.txt"), "hello from the project").expect("write");
    let host = HostTools::new(None, &project, &private, &control()).expect("tools");
    let value = Capabilities::observe(
        &host,
        "read_file",
        &serde_json::json!({"path":"note.txt"}),
        &control(),
    )
    .expect("project file is observable");
    assert!(value.to_string().contains("hello"));
    // There is no second area to read from any more; the world is one place.
    assert!(Capabilities::observe(
        &host,
        "list_dir",
        &serde_json::json!({"path": ".."}),
        &control()
    )
    .is_err());
    assert!(Capabilities::observe(
        &host,
        "write_file",
        &serde_json::json!({"path":"x.txt","text":"y"}),
        &control()
    )
    .is_err());
    assert!(Capabilities::observe(
        &host,
        "read_file",
        &serde_json::json!({"path":"../state/secret"}),
        &control()
    )
    .is_err());
    // An edit is an effect: the observer names the refusal instead of pretending
    // the tool is a connector.
    let edited = Capabilities::observe(
        &host,
        "edit_file",
        &serde_json::json!({"path":"note.txt","find":"hello","replace":"bye"}),
        &control(),
    );
    assert!(edited
        .unwrap_err()
        .to_string()
        .contains("edits belong to ajas"));
}
