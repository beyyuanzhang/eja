use eja::{
    turn_at, Action, ArtifactContent, ArtifactReader, Assignment, Brain, Decision, EjaContext,
    NoopRetriever, Report, ReportStatus, Store, TaskStatus, Worker,
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
            })
    }
}

struct QueueWorker {
    queue: Mutex<VecDeque<Report>>,
}

impl QueueWorker {
    fn new(reports: Vec<Report>) -> Self {
        Self {
            queue: Mutex::new(reports.into()),
        }
    }
}

impl Worker for QueueWorker {
    fn execute(&self, _assignment: &Assignment) -> Report {
        self.queue
            .lock()
            .expect("worker")
            .pop_front()
            .unwrap_or_else(|| Report::failed("no report"))
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
    }
}

fn done_report() -> Report {
    Report {
        status: ReportStatus::Done,
        summary: "checklist written".to_string(),
        artifacts: vec!["out.md".to_string()],
        evidence: vec!["out.md".to_string()],
        open_questions: Vec::new(),
        tools: Vec::new(),
    }
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
            vec![Action::ReadArtifact {
                path: "out.md".to_string(),
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
        &QueueWorker::new(vec![done_report()]),
        &NoopRetriever,
        &FakeReader,
        eja::now_utc(),
    )
    .expect("turn");

    assert_eq!(outcome.artifacts.len(), 1);
    assert_eq!(outcome.artifacts[0].path, "out.md");
    assert!(outcome.artifacts[0].text.contains("upload"));
    assert_eq!(outcome.tasks[0].status, TaskStatus::Done);
    assert!(outcome.tasks[0].close.as_ref().expect("close").accepted);
}
