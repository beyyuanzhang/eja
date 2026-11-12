use eja::adapters::{ArtifactReader, NoopRetriever, Ports};
use eja::{
    turn_at, Action, ArtifactContent, Assignment, Brain, Decision, EjaContext, LookSource,
    Selector, Store, TaskStatus, Worker,
};
use std::collections::VecDeque;
use std::sync::Mutex;

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
    fn decide(&self, _context: &EjaContext) -> Decision {
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
    fn execute(&self, _assignment: &Assignment) -> String {
        self.queue
            .lock()
            .expect("worker")
            .pop_front()
            .unwrap_or_else(|| {
                format!(
                    "0 steps, 0 tool calls, 0 artifacts, ended: error\n{}",
                    "no record"
                )
            })
    }
}

struct FakeReader;

impl ArtifactReader for FakeReader {
    fn read(&self, path: &str) -> ArtifactContent {
        ArtifactContent {
            path: path.to_string(),
            text: "build\nsmoke test\nupload\n".to_string(),
            truncated: false,
            error: None,
        }
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
        "1 steps, 1 tool calls, 1 artifacts, ended: message\n{}",
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
            log: Some(&NoopRetriever),
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
    use eja::adapters::FileArtifactReader;
    let dir = std::env::temp_dir().join(format!("eja-slice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let body: String = (0..20).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.join("note.txt"), body).expect("write");
    let reader = FileArtifactReader::new(&dir);
    let range = reader
        .slice("note.txt", &Selector::Range { from: 5, lines: 3 })
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
        )
        .expect("match");
    assert!(none["matches"].as_array().unwrap().is_empty());
}

#[test]
fn a_match_returns_every_hit_and_the_view_budget_is_the_only_limit() {
    use eja::adapters::FileArtifactReader;
    let dir = std::env::temp_dir().join(format!("eja-match-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let body: String = (0..30).map(|n| format!("hit {n}\n")).collect();
    std::fs::write(dir.join("note.txt"), body).expect("write");
    let reader = FileArtifactReader::new(&dir);
    let hit = reader
        .slice(
            "note.txt",
            &Selector::Match {
                text: "hit".to_string(),
                before: 0,
                after: 0,
            },
        )
        .expect("match");
    assert_eq!(hit["matches"].as_array().unwrap().len(), 30);
}
