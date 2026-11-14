//! Deterministic kernel benchmark. No network, no model calls.
//!
//! Run: cargo run --release --example kernel_bench

use chrono::{TimeZone, Utc};
use eja::adapters::FileArtifactReader;
use eja::adapters::{ArtifactReader, Ports};
use eja::{
    project, recover, turn_at, Action, ArtifactContent, Assignment, Brain, Decision, EjaContext,
    EventMeta, Kind, Result, Store, Worker,
};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct NoBrain;

impl Brain for NoBrain {
    fn decide(&self, _context: &EjaContext) -> Decision {
        Decision {
            reply: "ok".to_string(),
            actions: Vec::new(),
            refused: Vec::new(),
        }
    }
}

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
                reply: "ok".to_string(),
                actions: Vec::new(),
                refused: Vec::new(),
            })
    }
}

struct DoneWorker;

impl Worker for DoneWorker {
    fn execute(&self, _assignment: &Assignment) -> String {
        format!(
            "1 steps, 1 tool calls, 0 artifacts, ended: message\n{}",
            "done"
        )
    }
}

struct NoArtifacts;

impl ArtifactReader for NoArtifacts {
    fn read(&self, path: &str) -> ArtifactContent {
        ArtifactContent::failed(path, "benchmark")
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eja-bench-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0)
        .single()
        .expect("fixed time")
}

fn pct(samples: &[Duration], percentile: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut values: Vec<f64> = samples
        .iter()
        .map(|sample| sample.as_secs_f64() * 1000.0)
        .collect();
    values.sort_by(|a, b| a.total_cmp(b));
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

fn write_commitments(path: &Path, count: usize) {
    let mut file = File::create(path).expect("log");
    for id in 1..=count {
        let event = serde_json::json!({
            "id": id,
            "at": id as i64,
            "turn": id as u64 / 2 + 1,
            "scope": "eja",
            "kind": "commitment",
            "text": format!("commitment {id}"),
            "closes": null,
        });
        writeln!(file, "{}", event).expect("write event");
    }
    file.sync_all().expect("sync");
}

fn bench_project(root: &Path, events: usize, iterations: usize) -> (f64, f64, usize) {
    let dir = root.join(format!("project-{events}"));
    fs::create_dir_all(&dir).expect("dir");
    write_commitments(&dir.join("log.jsonl"), events);
    let store = Store::new(&dir).expect("store");
    let mut samples = Vec::new();
    for _ in 0..iterations {
        let start = Instant::now();
        let _context = project(&store, "benchmark", None, now()).expect("project");
        samples.push(start.elapsed());
    }
    let size = fs::metadata(dir.join("log.jsonl")).expect("metadata").len() as usize;
    (pct(&samples, 0.5), pct(&samples, 0.95), size)
}

fn bench_append(root: &Path, count: usize) -> (f64, f64, f64) {
    let dir = root.join(format!("append-{count}"));
    let store = Store::new(&dir).expect("store");
    let mut samples = Vec::new();
    let started = Instant::now();
    for index in 0..count {
        let start = Instant::now();
        store
            .append_simple(Kind::Commitment, &format!("c{index}"), 1, index as i64)
            .expect("append");
        samples.push(start.elapsed());
    }
    let total = started.elapsed().as_secs_f64();
    (pct(&samples, 0.5), pct(&samples, 0.95), total)
}

fn bench_empty_turns(root: &Path, count: usize) -> (f64, f64) {
    let dir = root.join("empty-turns");
    let store = Store::new(&dir).expect("store");
    let mut samples = Vec::new();
    for index in 0..count {
        let start = Instant::now();
        turn_at(
            &store,
            "ping",
            &NoBrain,
            &DoneWorker,
            &Ports {
                node: Some(&NoArtifacts),
                ..Default::default()
            },
            now() + chrono::Duration::seconds(index as i64),
        )
        .expect("turn");
        samples.push(start.elapsed());
    }
    (pct(&samples, 0.5), pct(&samples, 0.95))
}

fn bench_task_turns(root: &Path, count: usize) -> (f64, f64, usize) {
    let dir = root.join("task-turns");
    let store = Store::new(&dir).expect("store");
    let mut samples = Vec::new();
    for index in 0..count {
        let brain = QueueBrain::new(vec![
            Decision {
                reply: "start".to_string(),
                actions: vec![Action::OpenTask {
                    title: format!("task {index}"),
                    request: "do it".to_string(),
                    acceptance: "done".to_string(),
                    run: true,
                }],
                refused: Vec::new(),
            },
            Decision {
                reply: "accept".to_string(),
                actions: vec![Action::CloseTask {
                    task_id: index as u64 + 1,
                    accepted: true,
                    note: "ok".to_string(),
                }],
                refused: Vec::new(),
            },
        ]);
        let start = Instant::now();
        turn_at(
            &store,
            "run",
            &brain,
            &DoneWorker,
            &Ports {
                node: Some(&NoArtifacts),
                ..Default::default()
            },
            now() + chrono::Duration::seconds(index as i64 * 10),
        )
        .expect("turn");
        samples.push(start.elapsed());
    }
    let tasks = eja::tasks(&store.events().expect("events"));
    let done = tasks
        .iter()
        .filter(|task| task.status == eja::TaskStatus::Done)
        .count();
    (pct(&samples, 0.5), pct(&samples, 0.95), done)
}

fn bench_proposals(root: &Path, count: usize) -> f64 {
    let dir = root.join("proposals");
    let store = Store::new(&dir).expect("store");
    let started = Instant::now();
    for index in 0..count {
        let propose = QueueBrain::new(vec![Decision {
            reply: "propose".to_string(),
            actions: vec![Action::Propose {
                kind: eja::ProposalKind::Commitment,
                target_id: None,
                text: format!("commitment {index}"),
            }],
            refused: Vec::new(),
        }]);
        let outcome = turn_at(
            &store,
            "propose",
            &propose,
            &DoneWorker,
            &Ports {
                node: Some(&NoArtifacts),
                ..Default::default()
            },
            now() + chrono::Duration::seconds(index as i64 * 2),
        )
        .expect("propose");
        let proposal_id = outcome.context.proposals[0].id;
        let confirm = QueueBrain::new(vec![Decision {
            reply: "confirm".to_string(),
            actions: vec![Action::Confirm { proposal_id }],
            refused: Vec::new(),
        }]);
        eja::turn_at_with_options(
            &store,
            "confirm",
            &confirm,
            &DoneWorker,
            &Ports {
                node: Some(&NoArtifacts),
                ..Default::default()
            },
            now() + chrono::Duration::seconds(index as i64 * 2 + 1),
            &eja::TurnOptions {
                approvals: vec![proposal_id],
                ..Default::default()
            },
        )
        .expect("confirm");
    }
    started.elapsed().as_secs_f64() * 1000.0 / (count * 2) as f64
}

fn seed_interrupted(root: &Path, count: usize) -> Store {
    let dir = root.join("interrupted");
    let store = Store::new(&dir).expect("store");
    for index in 0..count {
        let task_id = index as u64 + 1;
        let assignment = Assignment {
            task_id,
            objective: "do it".to_string(),
            issued_at: 100,
            acceptance: String::new(),
            input: String::new(),
            resume: None,
        };
        store
            .append(
                Kind::TaskOpened,
                &format!("task:{task_id}"),
                "",
                1,
                EventMeta::data(
                    100,
                    serde_json::json!({
                    "title": format!("task {task_id}"),
                    "request": "do it",
                    "origin": "do it",
                    "acceptance": "done",
                                }),
                ),
            )
            .expect("open");
        store
            .append(
                Kind::TaskAssigned,
                &format!("task:{task_id}"),
                "",
                1,
                EventMeta::data(100, serde_json::to_value(&assignment).expect("assignment")),
            )
            .expect("assign");
    }
    store
}

fn bench_recovery(root: &Path, count: usize) -> (f64, usize) {
    let store = seed_interrupted(root, count);
    let start = Instant::now();
    let recovered = recover(&store, 2, 200).expect("recover");
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    (elapsed, recovered.len())
}

fn bench_artifact(root: &Path, count: usize) -> (f64, f64) {
    let dir = root.join("artifact");
    fs::create_dir_all(&dir).expect("dir");
    let content = "x".repeat(16 * 1024);
    fs::write(dir.join("artifact.txt"), content).expect("artifact");
    let reader = FileArtifactReader::with_limit(&dir, 16 * 1024);
    let mut samples = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let artifact = reader.read("artifact.txt");
        samples.push(start.elapsed());
        assert!(!artifact.truncated);
        assert_eq!(artifact.text.len(), 16 * 1024);
    }
    (pct(&samples, 0.5), pct(&samples, 0.95))
}

fn main() -> Result<()> {
    let root = temp_dir("all");
    println!("# EJA kernel benchmark");
    println!(
        "os={} arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!();

    println!("## project (read + fold + projection)");
    println!("| events | p50 ms | p95 ms | log bytes |");
    println!("|---:|---:|---:|---:|");
    for events in [1_000usize, 5_000, 10_000, 50_000] {
        let (p50, p95, size) = bench_project(&root, events, 20);
        println!("| {events} | {p50:.3} | {p95:.3} | {size} |");
    }

    println!();
    println!("## append + fsync");
    for count in [100usize, 500, 2_000] {
        let (append_p50, append_p95, append_total) = bench_append(&root, count);
        println!(
            "{count} appends: p50={append_p50:.3} ms, p95={append_p95:.3} ms, total={append_total:.3} s, {:.1} ops/s",
            count as f64 / append_total
        );
    }

    println!();
    println!("## turns");
    let (empty_p50, empty_p95) = bench_empty_turns(&root, 50);
    println!("empty turns: p50={empty_p50:.3} ms, p95={empty_p95:.3} ms");
    let (task_p50, task_p95, done) = bench_task_turns(&root, 20);
    println!("task turns: p50={task_p50:.3} ms, p95={task_p95:.3} ms, done={done}/20");

    println!();
    println!("## proposals");
    let proposal_ms = bench_proposals(&root, 50);
    println!("propose+confirm: {proposal_ms:.3} ms per turn (100 turns)");

    println!();
    println!("## recovery");
    let (recovery_ms, recovered) = bench_recovery(&root, 100);
    println!("100 interrupted tasks: {recovery_ms:.3} ms, recovered={recovered}");

    println!();
    println!("## artifact read");
    let (artifact_p50, artifact_p95) = bench_artifact(&root, 100);
    println!("16 KiB artifact: p50={artifact_p50:.3} ms, p95={artifact_p95:.3} ms");

    let _ = fs::remove_dir_all(&root);
    Ok(())
}
