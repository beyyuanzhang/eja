mod support;
use eja::adapters::{NodeReader, Ports};
use eja::{
    turn_at, Action, Assignment, Brain, Decision, EjaContext, LookSource, Selector, Store,
    TaskStatus, Worker,
};
use std::collections::VecDeque;
use std::sync::Mutex;
use support::control;

struct QueueBrain {
    queue: Mutex<VecDeque<Decision>>,
}

impl QueueBrain {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            queue: Mutex::new(decisions.into()),
        }
    }
}

impl Brain for QueueBrain {
    fn decide(&self, _context: &EjaContext, _: &eja::Control) -> Decision {
        self.queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: String::new(),
                actions: Vec::new(),
                refused: Vec::new(),
            })
    }
}

struct QueueWorker {
    queue: Mutex<VecDeque<String>>,
}

impl QueueWorker {
    fn new(reports: Vec<String>) -> Self {
        Self {
            queue: Mutex::new(reports.into()),
        }
    }
}

impl Worker for QueueWorker {
    fn execute(&self, _assignment: &Assignment, _: &eja::Control) -> String {
        self.queue
            .lock()
            .expect("worker")
            .pop_front()
            .unwrap_or_else(|| format!("0 steps, 0 tool calls, ended: error\n{}", "no record"))
    }
}

struct FakeReader;

impl NodeReader for FakeReader {
    fn slice(&self, path: &str, _: &Selector, _: &eja::Control) -> eja::Result<serde_json::Value> {
        Ok(serde_json::json!({"path":path,"text":"build\nsmoke test\nupload\n","truncated":false}))
    }
}

fn decision(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.to_string(),
        actions,
        refused: Vec::new(),
    }
}

fn record() -> String {
    format!(
        "1 steps, 1 tool calls, project: unchanged, ended: message\n{}",
        "checklist written"
    )
}

#[test]
fn eja_reads_artifact_content_then_decides() {
    let dir = std::env::temp_dir().join(format!("eja-artifact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain::new(vec![
        decision(
            "start",
            vec![Action::OpenTask {
                title: "release checklist".to_string(),
                request: "write a checklist".to_string(),
                acceptance: "contains build, smoke test, upload".to_string(),
                run: true,
            }],
        ),
        decision(
            "read",
            vec![Action::Look {
                source: LookSource::Node,
                target: "out.md".to_string(),
                arguments: serde_json::Value::Null,
                selector: Selector::Whole,
            }],
        ),
        decision(
            "accepted",
            vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "read it".to_string(),
            }],
        ),
    ]);
    let outcome = turn_at(
        &store,
        "do the task",
        &brain,
        &QueueWorker::new(vec![record()]),
        &Ports {
            node: Some(&FakeReader),
            ..Default::default()
        },
        eja::now_utc(),
    )
    .expect("turn");

    assert_eq!(outcome.evidence.len(), 1);
    assert_eq!(outcome.evidence[0].result["path"], "out.md");
    assert!(outcome.evidence[0].result["text"]
        .as_str()
        .expect("text")
        .contains("upload"));
    assert_eq!(outcome.tasks[0].status, TaskStatus::Done);
    assert!(outcome.tasks[0].close.as_ref().expect("close").accepted);
}

#[test]
fn node_files_answer_a_range_and_a_match() {
    use eja::adapters::NodeFiles;
    let dir = std::env::temp_dir().join(format!("eja-slice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let body: String = (0..20).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.join("note.txt"), body).expect("write");
    let reader = NodeFiles::new(&dir);
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
    use eja::adapters::NodeFiles;
    let dir = std::env::temp_dir().join(format!("eja-match-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let body: String = (0..30).map(|n| format!("hit {n}\n")).collect();
    std::fs::write(dir.join("note.txt"), body).expect("write");
    let reader = NodeFiles::new(&dir);
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
    let root = std::env::temp_dir().join(format!("eja-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let private = root.join("private");
    std::fs::create_dir_all(project.join("docs")).unwrap();
    std::fs::create_dir_all(&private).unwrap();
    let mut file = std::fs::File::create(project.join("README.md")).unwrap();
    file.write_all(b"hello").unwrap();
    let control = eja::adapters::Control::new(
        std::time::Duration::from_secs(5),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let ctx = ToolContext {
        node_id: 1,
        project: &project,
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
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn edit_file_replaces_one_exact_match_and_refuses_ambiguity() {
    use eja::adapters::{FileTools, ToolContext, ToolGateway};
    let root = std::env::temp_dir().join(format!("eja-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::write(work.join("a.txt"), "alpha beta gamma").unwrap();
    let control = eja::adapters::Control::new(
        std::time::Duration::from_secs(5),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    let ctx = ToolContext {
        node_id: 1,
        project: &work,
        private: &root.join("state"),
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
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn oversized_artifact_target_fails_before_content_trimming() {
    let reader = eja::adapters::NodeFiles::new(std::env::temp_dir());
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
    use eja::adapters::NodeFiles;
    let root = std::env::temp_dir().join(format!("eja-overlap-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hits.txt"), "hit 0\nhit 1\nhit 2\nhit 3\nhit 4\n").unwrap();
    let reader = NodeFiles::new(&root);
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
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn world_and_frozen_text_reads_share_encoding_bounds_and_cancellation() {
    use eja::adapters::{FileTools, NodeFiles, ToolContext, ToolGateway};
    let root = std::env::temp_dir().join(format!("eja-text-budget-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("escaped.txt"),
        [b'"', b'\\', 0, 0xff, b'\n'].repeat(60_000),
    )
    .unwrap();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let control = eja::Control::new(std::time::Duration::from_secs(5), cancelled.clone());
    let private = root.join(".eja");
    let context = ToolContext {
        node_id: 1,
        project: &root,
        private: &private,
        control: &control,
    };
    let reader = NodeFiles::new(&root);
    let node = reader
        .slice("escaped.txt", &Selector::Whole, &control)
        .unwrap();
    let world = FileTools
        .call(
            &context,
            "read_file",
            &serde_json::json!({"path":"escaped.txt"}),
        )
        .unwrap();
    assert_eq!(node["text"], world["text"]);
    for value in [node, world] {
        assert_eq!(value["truncated"], true);
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
    let _ = std::fs::remove_dir_all(root);
}
