//! Aja files, frozen records and tool reads: what a run may see, and how the
//! view budget bounds it.

mod support;

use eja::adapters::{Ports, RecordReader};
use eja::{turn_at, Action, Selector, TaskStatus};
use std::fmt::Write as _;
use support::{aja, control, decision, Done, Script, State};

/// The host's answer for a record read: fixed text, so the assertion is about
/// EJA's handling of the observation and never about a file on disk.
struct FakeReader;

impl RecordReader for FakeReader {
    fn slice(&self, path: &str, _: &Selector, _: &eja::Control) -> eja::Result<serde_json::Value> {
        Ok(serde_json::json!({"path":path,"text":"build\nsmoke test\nupload\n","truncated":false}))
    }
}

#[test]
fn eja_reads_artifact_content_then_decides() {
    let state = State::new("artifact-read");
    let store = state.store();
    let brain = Script::new(vec![
        decision(vec![aja("release checklist")]),
        decision(vec![Action::Look {
            target: "record:1/out.md".to_string(),
            arguments: serde_json::Value::Null,
            selector: Selector::Whole,
        }]),
        decision(vec![Action::CloseTask {
            task_id: 1,
            accepted: true,
            note: "read it".to_string(),
        }]),
    ]);
    let outcome = turn_at(
        &store,
        "do the task",
        &brain,
        &Done("checklist written"),
        &Ports {
            aja: Some(&FakeReader),
            ..Default::default()
        },
        eja::now_utc(),
    )
    .expect("turn");

    assert_eq!(outcome.context().evidence.len(), 1);
    assert_eq!(outcome.context().evidence[0].result["path"], "1/out.md");
    assert!(outcome.context().evidence[0].result["text"]
        .as_str()
        .expect("text")
        .contains("upload"));
    assert_eq!(outcome.context().tasks[0].status, TaskStatus::Done);
    assert!(
        outcome.context().tasks[0]
            .close
            .as_ref()
            .expect("close")
            .accepted
    );
}

#[test]
fn aja_files_answer_a_range_and_a_match() {
    use eja::adapters::RecordFiles;
    let state = State::new("slice");
    std::fs::create_dir_all(state.path()).expect("dir");
    let mut body = String::new();
    for n in 0..20 {
        writeln!(body, "line {n}").expect("string writer");
    }
    std::fs::write(state.path().join("note.txt"), body).expect("write");
    let reader = RecordFiles::new(state.path());
    let range = reader
        .slice(
            "note.txt",
            &Selector::Range { from: 5, lines: 3 },
            &control(),
        )
        .expect("range");
    assert_eq!(range["total_lines"], 20);
    assert_eq!(range["lines"][0]["line"], 5);
    assert_eq!(range["lines"][0]["text"], "line 5");
    assert_eq!(range["lines"].as_array().unwrap().len(), 3);
    let hit = reader
        .slice(
            "note.txt",
            &Selector::Match {
                text: "line 12".to_string(),
                before: 1,
                after: 1,
            },
            &control(),
        )
        .expect("match");
    assert_eq!(hit["matches"][0]["line"], 12);
    assert_eq!(hit["matches"][0]["context"].as_array().unwrap().len(), 3);
    let none = reader
        .slice(
            "note.txt",
            &Selector::Match {
                text: "absent".to_string(),
                before: 0,
                after: 0,
            },
            &control(),
        )
        .expect("match");
    assert!(none["matches"].as_array().unwrap().is_empty());
}

#[test]
fn a_match_returns_every_hit_and_the_view_budget_is_the_only_limit() {
    use eja::adapters::RecordFiles;
    let state = State::new("match");
    std::fs::create_dir_all(state.path()).expect("dir");
    let mut body = String::new();
    for n in 0..30 {
        writeln!(body, "hit {n}").expect("string writer");
    }
    std::fs::write(state.path().join("note.txt"), body).expect("write");
    let reader = RecordFiles::new(state.path());
    let hit = reader
        .slice(
            "note.txt",
            &Selector::Match {
                text: "hit".to_string(),
                before: 0,
                after: 0,
            },
            &control(),
        )
        .expect("match");
    assert_eq!(hit["matches"].as_array().unwrap().len(), 30);
}

#[test]
fn list_dir_reads_the_project_and_reports_what_it_hid() {
    use eja::adapters::{FileTools, ToolContext, ToolGateway};
    use std::io::Write;
    let state = State::new("list");
    let project = state.path().join("project");
    let private = state.path().join("private");
    std::fs::create_dir_all(project.join("docs")).unwrap();
    std::fs::create_dir_all(&private).unwrap();
    let mut file = std::fs::File::create(project.join("README.md")).unwrap();
    file.write_all(b"hello").unwrap();
    let control = control();
    let ctx = ToolContext {
        aja_id: 1,
        workspace: Some(&project),
        private: &private,
        control: &control,
    };
    let listing = FileTools
        .call(&ctx, "list_dir", &serde_json::json!({"path": "."}))
        .unwrap();
    let entries = listing["entries"].as_array().unwrap();
    assert!(entries
        .iter()
        .any(|e| e.as_str().unwrap().starts_with("README.md")));
    assert!(entries.iter().any(|e| e.as_str().unwrap() == "docs/"));
    assert_eq!(listing["truncated"], false);
}

#[test]
fn edit_file_replaces_one_exact_match_and_refuses_ambiguity() {
    use eja::adapters::{FileTools, ToolContext, ToolGateway};
    let state = State::new("edit");
    let work = state.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let private = state.path().join("state");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(work.join("a.txt"), "alpha beta gamma").unwrap();
    let control = control();
    let ctx = ToolContext {
        aja_id: 1,
        workspace: Some(&work),
        private: &private,
        control: &control,
    };
    let ok = FileTools
        .call(
            &ctx,
            "edit_file",
            &serde_json::json!({"path": "a.txt", "find": "beta", "replace": "B"}),
        )
        .unwrap();
    assert_eq!(ok["edited"], "a.txt");
    assert_eq!(
        std::fs::read_to_string(work.join("a.txt")).unwrap(),
        "alpha B gamma"
    );
    let missing = match FileTools.call(
        &ctx,
        "edit_file",
        &serde_json::json!({"path": "a.txt", "find": "zzz", "replace": "x"}),
    ) {
        Ok(_) => panic!("a missing match must not edit"),
        Err(error) => error.to_string(),
    };
    assert!(missing.contains("exactly once"), "{missing}");
    std::fs::write(work.join("b.txt"), "same same").unwrap();
    let ambiguous = match FileTools.call(
        &ctx,
        "edit_file",
        &serde_json::json!({"path": "b.txt", "find": "same", "replace": "x"}),
    ) {
        Ok(_) => panic!("an ambiguous match must not edit"),
        Err(error) => error.to_string(),
    };
    assert!(ambiguous.contains("found 2"), "{ambiguous}");
}

#[test]
fn file_tools_reach_the_reserved_worktree_but_not_its_private_parent() {
    use eja::adapters::{FileTools, ToolContext, ToolGateway};
    use serde_json::json;

    let fixture = State::new("isolated-file-tools");
    let private = fixture.path().join("state");
    let worktree = private.join("worlds/0/1");
    std::fs::create_dir_all(&worktree).unwrap();
    let journal = private.join("log.jsonl");
    std::fs::write(&journal, "host facts").unwrap();
    let tool_control = control();
    let tools = ToolContext {
        aja_id: 1,
        workspace: Some(&worktree),
        private: &private,
        control: &tool_control,
    };
    FileTools
        .call(
            &tools,
            "write_file",
            &json!({"path":"out.txt","text":"output"}),
        )
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(worktree.join("out.txt")).unwrap(),
        "output"
    );
    let found = FileTools
        .call(&tools, "glob", &json!({"pattern":"*.txt"}))
        .unwrap();
    assert_eq!(found["matches"], json!(["out.txt"]));
    assert!(FileTools
        .call(&tools, "read_file", &json!({"path":journal}))
        .is_err());
    assert!(FileTools
        .call(&tools, "read_file", &json!({"path":"../../../log.jsonl"}))
        .is_err());
}

#[test]
fn oversized_artifact_target_fails_before_content_trimming() {
    let state = State::new("oversized");
    std::fs::create_dir_all(state.path()).unwrap();
    let reader = eja::adapters::RecordFiles::new(state.path());
    let started = std::time::Instant::now();
    assert!(reader
        .slice(
            &"x".repeat(eja::aja::FRAME_LIMIT + 1),
            &Selector::Whole,
            &control()
        )
        .is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn overlapping_match_windows_follow_hit_positions_including_the_file_end() {
    use eja::adapters::RecordFiles;
    let state = State::new("overlap");
    std::fs::create_dir_all(state.path()).unwrap();
    std::fs::write(
        state.path().join("hits.txt"),
        "hit 0\nhit 1\nhit 2\nhit 3\nhit 4\n",
    )
    .unwrap();
    let reader = RecordFiles::new(state.path());
    let value = reader
        .slice(
            "hits.txt",
            &Selector::Match {
                text: "hit".into(),
                before: 1,
                after: 2,
            },
            &control(),
        )
        .unwrap();
    let contexts: Vec<Vec<u64>> = value["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            hit["context"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| line["line"].as_u64().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        contexts,
        vec![
            vec![0, 1, 2],
            vec![0, 1, 2, 3],
            vec![1, 2, 3, 4],
            vec![2, 3, 4],
            vec![3, 4]
        ]
    );
    assert_eq!(value["truncated"], false);
}

#[test]
fn world_and_frozen_text_reads_share_encoding_bounds_and_cancellation() {
    use eja::adapters::{FileTools, RecordFiles, ToolContext, ToolGateway};
    let state = State::new("text-budget");
    std::fs::create_dir_all(state.path()).unwrap();
    std::fs::write(
        state.path().join("escaped.txt"),
        [b'"', b'\\', 0, 0xff, b'\n'].repeat(60_000),
    )
    .unwrap();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let control = control().cancelled_by(cancelled.clone());
    let private = state.path().join(".eja");
    let context = ToolContext {
        aja_id: 1,
        workspace: Some(state.path()),
        private: &private,
        control: &control,
    };
    let reader = RecordFiles::new(state.path());
    let aja = reader
        .slice("escaped.txt", &Selector::Whole, &control)
        .unwrap();
    let world = FileTools
        .call(
            &context,
            "read_file",
            &serde_json::json!({"path":"escaped.txt"}),
        )
        .unwrap();
    assert_eq!(aja["text"], world["text"]);
    for value in [aja, world] {
        assert_eq!(value["truncated"], true);
        // The frame cap bounds the serialized page, so measure it as sent.
        assert!(value.to_string().len() <= eja::aja::FRAME_LIMIT);
        assert!(value["text"].as_str().unwrap().contains('\u{fffd}'));
    }
    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader
        .slice("escaped.txt", &Selector::Whole, &control)
        .is_err());
    assert!(FileTools
        .call(
            &context,
            "read_file",
            &serde_json::json!({"path":"escaped.txt"})
        )
        .is_err());
}
