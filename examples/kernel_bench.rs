#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)] // a benchmark converts measured counts for printing
//! Deterministic kernel benchmark. No network, no model calls.
//!
//! Run: cargo run --release --example `kernel_bench`

use chrono::{TimeZone, Utc};
use eja::adapters::RecordFiles;
use eja::adapters::{Ports, RecordReader};
use eja::{
    inspect_journal, recover, turn_at, Action, Assignment, Brain, Decision, EjaContext, EventMeta,
    Kind, State, Store, Worker,
};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[path = "../src/host/state.rs"]
#[allow(dead_code)]
mod host_state;

use host_state::{state_json, Cache, Health};

struct NoBrain;

impl Brain for NoBrain {
    fn decide(&self, _context: &EjaContext, _: &eja::Control) -> eja::Result<Decision> {
        Ok(Decision {
            reply: "ok".to_string(),
            choices: Vec::new(),
            actions: Vec::new(),
            refused: Vec::new(),
        })
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
    fn decide(&self, _context: &EjaContext, _: &eja::Control) -> eja::Result<Decision> {
        Ok(self
            .queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: "ok".to_string(),
                choices: Vec::new(),
                actions: Vec::new(),
                refused: Vec::new(),
            }))
    }
}

struct DoneWorker;

impl Worker for DoneWorker {
    fn execute(&self, _assignment: &Assignment, _: &eja::Control) -> String {
        format!(
            "1 steps, 1 tool calls, 0 artifacts, ended: message\n{}",
            "done"
        )
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
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

fn write_commitments(path: &Path, count: usize) {
    let root = path.parent().expect("state directory");
    fs::create_dir_all(root).expect("state directory");
    let initial = Store::new(root).expect("initialize state");
    assert!(initial.events().expect("events").is_empty());
    drop(initial);
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("committed log");
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
        writeln!(file, "{event}").expect("write event");
    }
    file.sync_all().expect("sync");
    let checked = Store::new(root).expect("reopen benchmark state");
    assert_eq!(checked.events().expect("events").len(), count);
}

struct Scale {
    read_p50: f64,
    read_p95: f64,
    fold_p50: f64,
    fold_p95: f64,
    http_p50: f64,
    http_p95: f64,
    size: usize,
    peak_kib: u64,
}

fn peak_rss_kib() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` initializes the structure on success; its return value
    // is checked before the value is read.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    // SAFETY: the successful call above initialized the structure.
    let usage = unsafe { usage.assume_init() };
    let peak = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") {
        peak / 1024
    } else {
        peak
    }
}

fn bench_scale(root: &Path, events: usize, iterations: usize) -> Scale {
    let dir = root.join(format!("project-{events}"));
    fs::create_dir_all(&dir).expect("dir");
    write_commitments(&dir.join("log.jsonl"), events);
    let store = Store::new(&dir).expect("store");
    let events = store.events().expect("events");
    let health = Health {
        model: false,
        model_name: String::new(),
        state: true,
        tools: false,
        journal: "ok",
        journal_error: None,
        torn: false,
        turn_in_flight: false,
        capability_errors: Vec::new(),
    };
    let mut read = Vec::new();
    let mut fold = Vec::new();
    let mut http = Vec::new();
    for _ in 0..iterations {
        let start = Instant::now();
        let scan = inspect_journal(&store.log_path).expect("scan");
        std::hint::black_box(scan.events.len());
        read.push(start.elapsed());

        let start = Instant::now();
        let state = State::fold(events.clone());
        std::hint::black_box(state.events().len());
        fold.push(start.elapsed());

        let start = Instant::now();
        let mut cache = Cache::new(dir.clone());
        let view = state_json(&mut cache, 0, &health, false, None);
        std::hint::black_box(view["lastEvent"].as_u64());
        http.push(start.elapsed());
    }
    let size = fs::metadata(dir.join("log.jsonl")).expect("metadata").len() as usize;
    Scale {
        read_p50: pct(&read, 0.5),
        read_p95: pct(&read, 0.95),
        fold_p50: pct(&fold, 0.5),
        fold_p95: pct(&fold, 0.95),
        http_p50: pct(&http, 0.5),
        http_p95: pct(&http, 0.95),
        size,
        peak_kib: peak_rss_kib(),
    }
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
            &Ports::default(),
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
                choices: Vec::new(),
                actions: vec![Action::OpenTask {
                    title: format!("task {index}"),
                    request: "do it".to_string(),
                    acceptance: "done".to_string(),
                    workspace: None,
                    evidence: None,
                    run: true,
                }],
                refused: Vec::new(),
            },
            Decision {
                reply: "accept".to_string(),
                choices: Vec::new(),
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
            &Ports::default(),
            now() + chrono::Duration::seconds(index as i64 * 10),
        )
        .expect("turn");
        samples.push(start.elapsed());
    }
    let tasks = State::load(&store).expect("state").tasks().to_vec();
    let done = tasks
        .iter()
        .filter(|task| task.status == eja::TaskStatus::Done)
        .count();
    (pct(&samples, 0.5), pct(&samples, 0.95), done)
}

/// A turn reads the journal once and validates every action against that fold, so
/// a decision with many reads costs one read plus one commit per action rather than
/// one full re-read and re-fold per action. The number that must stay flat is the
/// cost of one action as the log grows.
fn bench_turn_actions(root: &Path, events: usize, actions: usize) -> f64 {
    let dir = root.join(format!("turn-actions-{events}-{actions}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir");
    write_commitments(&dir.join("log.jsonl"), events);
    let store = Store::new(&dir).expect("store");
    let look = |index: usize| Action::Look {
        target: format!("log:event:{}", index + 1),
        arguments: serde_json::Value::Null,
        selector: eja::Selector::Whole,
    };
    let brain = QueueBrain::new(vec![
        Decision {
            reply: "read".to_string(),
            choices: Vec::new(),
            actions: (0..actions).map(look).collect(),
            refused: Vec::new(),
        },
        Decision {
            reply: "done".to_string(),
            choices: Vec::new(),
            actions: Vec::new(),
            refused: Vec::new(),
        },
    ]);
    let start = Instant::now();
    turn_at(
        &store,
        "inspect",
        &brain,
        &DoneWorker,
        &Ports::default(),
        now(),
    )
    .expect("turn");
    start.elapsed().as_secs_f64() * 1000.0
}

fn bench_proposals(root: &Path, count: usize) -> f64 {
    let dir = root.join("proposals");
    let store = Store::new(&dir).expect("store");
    let started = Instant::now();
    for index in 0..count {
        let propose = QueueBrain::new(vec![Decision {
            reply: "propose".to_string(),
            choices: Vec::new(),
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
            &Ports::default(),
            now() + chrono::Duration::seconds(index as i64 * 2),
        )
        .expect("propose");
        let proposal_id = outcome.context().proposals[0].id;
        let confirm = QueueBrain::new(vec![Decision {
            reply: "confirm".to_string(),
            choices: Vec::new(),
            actions: vec![Action::Confirm { proposal_id }],
            refused: Vec::new(),
        }]);
        eja::turn_at_with_options(
            &store,
            "confirm",
            &confirm,
            &DoneWorker,
            &Ports::default(),
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
            workspace: String::new(),
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
    let reader = RecordFiles::with_limit(&dir, 16 * 1024);
    let mut samples = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let artifact = reader
            .slice(
                "artifact.txt",
                &eja::Selector::Whole,
                &eja::Control::new(Duration::from_secs(5), std::sync::Arc::default()),
            )
            .unwrap();
        samples.push(start.elapsed());
        assert_eq!(artifact["truncated"], false);
        assert_eq!(artifact["text"].as_str().unwrap().len(), 16 * 1024);
    }
    (pct(&samples, 0.5), pct(&samples, 0.95))
}

fn main() {
    let root = temp_dir("all");
    println!("# EJA kernel benchmark");
    println!(
        "os={} arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!();

    println!("## history scale");
    println!(
        "| events | read p50/p95 ms | fold p50/p95 ms | http refresh p50/p95 ms | log bytes | peak RSS KiB |"
    );
    println!("|---:|---:|---:|---:|---:|---:|");
    for events in [1_000usize, 5_000, 10_000, 50_000] {
        let iterations = match events {
            1_000 => 20,
            5_000 => 10,
            10_000 => 5,
            _ => 3,
        };
        let scale = bench_scale(&root, events, iterations);
        println!(
            "| {events} | {:.3} / {:.3} | {:.3} / {:.3} | {:.3} / {:.3} | {} | {} |",
            scale.read_p50,
            scale.read_p95,
            scale.fold_p50,
            scale.fold_p95,
            scale.http_p50,
            scale.http_p95,
            scale.size,
            scale.peak_kib
        );
    }
    println!();
    println!("Peak RSS is the process high-water mark after each row; it is monotonic.");

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
    println!("## a turn with 200 reads (one journal read, one fold)");
    for events in [1_000usize, 10_000, 50_000] {
        let ms = bench_turn_actions(&root, events, 200);
        println!("{events} events: {ms:.1} ms");
    }

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
}
