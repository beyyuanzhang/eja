mod support;

use eja::adapters::{Capabilities, Ports};
use eja::{Action, Decision, Kind, Result, Selector};
use support::{control, decision, Done, Script, State};

/// A decision whose reply the turn states. The kit's `decision` always says
/// "structure"; this file asserts what the turn answered.
fn say(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.to_string(),
        choices: Vec::new(),
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
fn an_environment_look_does_not_depend_on_waiting_tasks() {
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
        // The environment is read while judging, so the task is opened and run first.
        decision(vec![Action::OpenTask {
            title: "sky".into(),
            request: "check the sky".into(),
            acceptance: String::new(),
            workspace: None,
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
    use eja::adapters::{ToolRegistry, WorkspaceTools};
    let fixture = State::new("observe-fs");
    let project = fixture.path().join("project");
    let private = fixture.path().join("state");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&private).expect("private");
    std::fs::write(project.join("note.txt"), "hello from the project").expect("write");
    let host = ToolRegistry::new(
        &private,
        vec![Box::new(WorkspaceTools::new(
            true,
            true,
            Some(project.clone()),
            vec![project],
            &private,
        ))],
    )
    .expect("tools");
    let value = Capabilities::observe(
        &host,
        "read_file",
        &serde_json::json!({"path":"note.txt"}),
        &control(),
    )
    .expect("project file is observable");
    assert!(value.to_string().contains("hello"));
    // There is no second area to read from any more; the environment is one place.
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

#[test]
fn the_host_reads_every_granted_workspace_and_nothing_else() {
    use eja::adapters::{ToolRegistry, WorkspaceTools};
    let fixture = State::new("observe-grants");
    let alpha = fixture.path().join("alpha");
    let beta = fixture.path().join("beta");
    let private = fixture.path().join("state");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(alpha.join("a.txt"), "in alpha").unwrap();
    std::fs::write(beta.join("b.txt"), "in beta").unwrap();
    std::fs::write(private.join("secret.txt"), "private").unwrap();
    let host = ToolRegistry::new(
        &private,
        vec![Box::new(WorkspaceTools::new(
            true,
            true,
            Some(alpha.clone()),
            vec![alpha, beta.clone()],
            &private,
        ))],
    )
    .expect("tools");
    let value = Capabilities::observe(
        &host,
        "read_file",
        &serde_json::json!({"path":"b.txt"}),
        &control(),
    )
    .expect("a granted workspace is readable");
    assert!(value.to_string().contains("in beta"));
    let value = Capabilities::observe(
        &host,
        "read_file",
        &serde_json::json!({"path": beta.join("b.txt").display().to_string()}),
        &control(),
    )
    .expect("absolute paths inside a grant resolve");
    assert!(value.to_string().contains("in beta"));
    assert!(
        Capabilities::observe(
            &host,
            "read_file",
            &serde_json::json!({"path": private.join("secret.txt").display().to_string()}),
            &control(),
        )
        .is_err(),
        "the private state stays unreadable"
    );
    assert!(
        Capabilities::observe(
            &host,
            "read_file",
            &serde_json::json!({"path":"../state/secret.txt"}),
            &control(),
        )
        .is_err(),
        "traversal outside the grants is refused"
    );
}

#[test]
fn the_workspace_grant_switches_off_every_entry() {
    use eja::adapters::{ToolProvider, ToolRegistry, WorkspaceTools};
    let fixture = State::new("observe-switch");
    let project = fixture.path().join("project");
    let private = fixture.path().join("state");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&private).expect("private");
    std::fs::write(project.join("note.txt"), "hello").expect("write");
    let host = ToolRegistry::new(
        &private,
        vec![Box::new(WorkspaceTools::new(
            false,
            false,
            Some(project.clone()),
            vec![project.clone()],
            &private,
        ))],
    )
    .expect("tools");
    assert!(host
        .definitions(&eja::Assignment {
            task_id: 1,
            objective: String::new(),
            workspace: String::new(),
            acceptance: String::new(),
            input: String::new(),
            resume: None,
            issued_at: 0,
        })
        .is_empty());
    let context = eja::adapters::ToolContext {
        aja_id: 1,
        workspace: Some(&project),
        private: &private,
        control: &control(),
    };
    assert!(host
        .call(
            &context,
            "read_file",
            &serde_json::json!({"path":"note.txt"})
        )
        .is_err());
    assert!(Capabilities::observe(
        &host,
        "read_file",
        &serde_json::json!({"path":"note.txt"}),
        &control()
    )
    .is_err());
    assert!(Capabilities::witness(
        &host,
        &serde_json::json!({"tool":"read_file","arguments":{"path":"note.txt"}}),
        &control()
    )
    .is_err());
}
