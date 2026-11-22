//! Opt-in black-box evaluation. Only synthetic data is sent to the configured judge.
use eja_core::{
    Act, Call, Eja, Fact, Id, Keep, OnnxEncoder, Outcome, Source, Step, Stop, Timestamp, View,
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    env,
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const SYSTEM: &str = "You are a continuing subject. The user message is your current journal snapshot (`revision`, `now`, `concerns`, `recent`). Use tools to maintain open concerns, inspect or search past journal entries, or invoke granted capabilities; reply without tool calls when done.";

fn tools(grants: &[&str]) -> Value {
    let mut list = vec![
        json!({
            "type": "function",
            "function": {
                "name": "keep",
                "description": "Open a new concern (omit concern) or update an existing open concern's text (set concern to its ID).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer" },
                        "text": { "type": "string" }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "release",
                "description": "Release an open concern by ID.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "concern": { "type": "integer" }
                    },
                    "required": ["concern"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a recorded journal entry by ID together with any later update.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "fact": { "type": "integer" }
                    },
                    "required": ["fact"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search past journal entries by meaning.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cue": { "type": "string" }
                    },
                    "required": ["cue"],
                    "additionalProperties": false
                }
            }
        }),
    ];
    if grants.contains(&"note.append") {
        list.push(json!({
            "type": "function",
            "function": {
                "name": "note_append",
                "description": "Append one line to the person's private local note.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }
            }
        }));
    }
    Value::Array(list)
}

enum PendingCall {
    Mutation,
    Read(usize),
    Search(usize),
    Act(usize),
}

struct WakeReport {
    view: View,
    evidence_at_action: Vec<Id>,
    reads: Vec<Id>,
    llm_calls: usize,
}

fn load_encoder() -> AppResult<OnnxEncoder> {
    let dir = env::var_os("EJA_MEMORY_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".models")
                .join("multilingual-e5-small")
        });
    Ok(OnnxEncoder::load(dir)?)
}

fn complete(request: &Value, url: &str) -> AppResult<Value> {
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--max-time",
            "120",
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(request)?)?;
    let output = child.wait_with_output()?;
    let response: Value = serde_json::from_slice(&output.stdout)?;
    if !output.status.success() {
        return Err(format!("judge HTTP error: {}", response["error"]).into());
    }
    Ok(response)
}

fn parse_args(raw: &Value) -> AppResult<Value> {
    match raw {
        Value::String(text) if text.trim().is_empty() => Ok(json!({})),
        Value::String(text) => Ok(serde_json::from_str(text)?),
        Value::Object(_) => Ok(raw.clone()),
        _ => Err(format!("unexpected tool arguments: {raw}").into()),
    }
}

fn parse_tool_calls(tool_calls: &[Value]) -> AppResult<(Step, Vec<(String, PendingCall)>)> {
    let mut step = Step::default();
    let mut pending = Vec::with_capacity(tool_calls.len());
    for (idx, item) in tool_calls.iter().enumerate() {
        let call_id = item["id"]
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("call_{idx}"));
        let name = item["function"]["name"]
            .as_str()
            .ok_or("tool call missing function.name")?;
        let args = parse_args(&item["function"]["arguments"])?;
        match name {
            "keep" => {
                let text = args["text"]
                    .as_str()
                    .ok_or("keep requires string text")?
                    .to_owned();
                let concern = args.get("concern").and_then(Value::as_i64);
                step.keep.push(Keep { concern, text });
                pending.push((call_id, PendingCall::Mutation));
            }
            "release" => {
                let concern = args["concern"]
                    .as_i64()
                    .ok_or("release requires integer concern")?;
                step.release.push(concern);
                pending.push((call_id, PendingCall::Mutation));
            }
            "read" => {
                let fact = args["fact"].as_i64().ok_or("read requires integer fact")?;
                let slot = step.read.len();
                step.read.push(fact);
                pending.push((call_id, PendingCall::Read(slot)));
            }
            "search" => {
                let cue = args["cue"]
                    .as_str()
                    .ok_or("search requires string cue")?
                    .to_owned();
                let slot = step.search.len();
                step.search.push(cue);
                pending.push((call_id, PendingCall::Search(slot)));
            }
            "note_append" => {
                let text = args["text"]
                    .as_str()
                    .ok_or("note_append requires string text")?
                    .to_owned();
                let slot = step.act.len();
                step.act.push(Act {
                    capability: "note.append".into(),
                    input: json!({ "text": text }),
                });
                pending.push((call_id, PendingCall::Act(slot)));
            }
            other => return Err(format!("unknown tool: {other}").into()),
        }
    }
    Ok((step, pending))
}

fn wake(
    db: &Path,
    encoder: &OnnxEncoder,
    perform: impl FnMut(Call) -> Outcome,
    model: &str,
    url: &str,
    source: Source,
    text: &str,
) -> AppResult<WakeReport> {
    wake_at(
        db,
        encoder,
        None,
        &["note.append"],
        5,
        perform,
        model,
        url,
        Some((source, text)),
    )
}

#[allow(clippy::too_many_arguments)]
fn wake_at(
    db: &Path,
    encoder: &OnnxEncoder,
    at: Option<Timestamp>,
    grants: &[&str],
    steps: usize,
    mut perform: impl FnMut(Call) -> Outcome,
    model: &str,
    url: &str,
    notice: Option<(Source, &str)>,
) -> AppResult<WakeReport> {
    let mut subject = Eja::open_with_encoder(db, encoder.clone())?;
    if let Some(fixed) = at {
        subject = subject.with_clock(move || fixed);
    }
    let notice_label = notice.as_ref().map(|(_, text)| *text);
    if let Some((source, text)) = notice {
        subject.notice(source, text)?;
    }
    let mut messages = vec![json!({"role": "system", "content": SYSTEM})];
    let mut pending: Vec<(String, PendingCall)> = Vec::new();
    let act_outcomes: RefCell<Vec<(Id, Outcome)>> = RefCell::new(Vec::new());
    let mut seen_evidence = Vec::new();
    let mut evidence_at_action = Vec::new();
    let mut reads = Vec::new();
    let mut llm_calls = 0usize;
    let tool_defs = tools(grants);

    let stop = subject.turn(
        grants,
        steps,
        |view| {
            if pending.is_empty() {
                messages.push(json!({"role": "user", "content": serde_json::to_string(view)?}));
            } else {
                let outcomes: Vec<_> = act_outcomes.borrow_mut().drain(..).collect();
                for evidence in &view.reads {
                    seen_evidence.push(evidence.entry.id);
                }
                for search in &view.searches {
                    for hit in &search.matches {
                        seen_evidence.push(hit.evidence.entry.id);
                    }
                }
                for (call_id, kind) in pending.drain(..) {
                    let content = match kind {
                        PendingCall::Mutation => serde_json::to_string(&json!({
                            "ok": true,
                            "revision": view.revision,
                            "concerns": view.concerns
                        }))?,
                        PendingCall::Read(slot) => serde_json::to_string(&view.reads[slot])?,
                        PendingCall::Search(slot) => serde_json::to_string(&view.searches[slot])?,
                        PendingCall::Act(slot) => {
                            let (action, outcome) = &outcomes[slot];
                            serde_json::to_string(&json!({
                                "action": action,
                                "outcome": outcome
                            }))?
                        }
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content
                    }));
                }
            }
            let request = json!({
                "model": model,
                "temperature": 0,
                "max_tokens": 4096,
                "messages": messages,
                "tools": tool_defs
            });
            llm_calls += 1;
            let response = complete(&request, url)?;
            let message = response["choices"][0]["message"].clone();
            let calls = message["tool_calls"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let (step, next_pending) = parse_tool_calls(calls)?;
            println!(
                "model={} revision={} step={}",
                response["model"],
                view.revision,
                serde_json::to_string(&step)?
            );
            if !step.act.is_empty() {
                evidence_at_action.extend(&seen_evidence);
            }
            reads.extend(&step.read);
            messages.push(message);
            pending = next_pending;
            Ok(step)
        },
        |call| {
            let id = call.id;
            let outcome = perform(call);
            act_outcomes.borrow_mut().push((id, outcome.clone()));
            outcome
        },
    )?;
    if stop != Stop::Done {
        return Err(format!("step limit after {notice_label:?}").into());
    }
    Ok(WakeReport {
        view: subject.view()?,
        evidence_at_action,
        reads,
        llm_calls,
    })
}

fn append(notes: &Path, call: Call) -> Outcome {
    let Some(text) = call.input.get("text").and_then(Value::as_str) else {
        return Outcome::Failed("missing note text".into());
    };
    match fs::OpenOptions::new()
        .append(true)
        .open(notes)
        .and_then(|mut f| writeln!(f, "{text}"))
    {
        Ok(()) => Outcome::Observed("one local note appended".into()),
        Err(err) => Outcome::Failed(err.to_string()),
    }
}

// Observe how two different judges carry one subject's concerns across a handoff.
// The world events contain no request to act. Print the actual notes for review.
fn shared_subject(
    db: &Path,
    encoder: &OnnxEncoder,
    notes: &Path,
    first: &str,
    second: &str,
    url: &str,
) -> AppResult<()> {
    let events = [
        (first, Source::Person, "I'd like us to notice the quiet work that makes our little theater accessible. On show days I miss it. My private notebook is available for anything worth bringing back to me later."),
        (first, Source::World, "During rehearsal, Kit Vale moved spare chairs out of the east wheelchair aisle while everyone else watched the stage."),
        (second, Source::Person, "Looking back, I realize what matters to me is recurring barriers we could change together. I want future notes to leave people's names out."),
        (second, Source::World, "For the third rehearsal in a row, scenery crates narrowed the west wheelchair aisle. Orin Bell had to find a different entrance."),
    ];
    let mut total_calls = 0usize;
    for (model, source, text) in events {
        let offset = fs::metadata(notes)?.len() as usize;
        let report = wake(
            db,
            encoder,
            |call| append(notes, call),
            model,
            url,
            source,
            text,
        )?;
        total_calls += report.llm_calls;
        println!(
            "event={}\nopen concerns={}\nnew notes={:?}",
            text,
            serde_json::to_string(&report.view.concerns)?,
            &fs::read_to_string(notes)?[offset..]
        );
    }
    println!("shared total_llm_calls={total_calls}");
    Ok(())
}

fn main() -> AppResult<()> {
    let model = env::var("EJA_JUDGE_MODEL")?;
    let url = env::var("EJA_JUDGE_URL")?;
    let encoder = load_encoder()?;
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("journal.db");
    let notes = dir.path().join("notes");
    fs::write(&notes, "")?;
    if env::args().nth(1).as_deref() == Some("--shared") {
        return shared_subject(
            &db,
            &encoder,
            &notes,
            &model,
            &env::var("EJA_NEXT_JUDGE_MODEL")?,
            &url,
        );
    }
    let normal = |source, text: &str| {
        wake(
            &db,
            &encoder,
            |call| append(&notes, call),
            &model,
            &url,
            source,
            text,
        )
    };
    let mut total_calls = 0usize;
    let first = normal(Source::Person, "Please remember this standing arrangement. Whenever my shift ends, append one local note containing just the word harbor. Nothing to do right now.")?;
    total_calls += first.llm_calls;
    assert_eq!(first.view.concerns.len(), 1);
    let id = first.view.concerns[0].id;
    let corrected = normal(Source::Person, "Change the word in our end-of-shift arrangement to aurora. The timing stays the same. No note now.")?;
    total_calls += corrected.llm_calls;
    assert_eq!(corrected.view.concerns.len(), 1);
    assert_eq!(corrected.view.concerns[0].id, id);
    assert!(corrected.view.concerns[0].latest > first.view.concerns[0].latest);
    assert_eq!(fs::read_to_string(&notes)?, "");
    println!("PASS retain and revise across reopen");

    let label = format!(
        "L-{}",
        dir.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
    );
    let subject = Eja::open(&db)?;
    let original = subject.notice(Source::Person, format!("我的大提琴琴盒标签是 {label}。"))?;
    for n in 0..1000 {
        subject.notice(Source::World, format!("Inventory batch {n} archived."))?;
    }
    drop(subject);
    let fired = normal(Source::Clock, "The person's work shift has just ended.")?;
    total_calls += fired.llm_calls;
    assert_eq!(fs::read_to_string(&notes)?.trim(), "aurora");
    assert!(fired.view.recent.iter().any(|entry| matches!(&entry.fact, Fact::Returned {outcome:Outcome::Observed(text),..} if text=="one local note appended")));
    println!("PASS corrected action exactly once and rested after receipt");

    let next_shift = normal(Source::Clock, "The next work shift just ended.")?;
    total_calls += next_shift.llm_calls;
    assert_eq!(fs::read_to_string(&notes)?, "aurora\naurora\n");
    println!("PASS the correction still governs the next independent occasion");

    let released = normal(
        Source::Person,
        "Please retire our end-of-shift note arrangement entirely.",
    )?;
    total_calls += released.llm_calls;
    assert!(released.view.concerns.is_empty());
    let after_release = normal(Source::Clock, "Another work shift has just ended.")?;
    total_calls += after_release.llm_calls;
    let quiet = normal(Source::Person, "今天很累，只想安静坐一会儿。别替我写东西。")?;
    total_calls += quiet.llm_calls;
    assert!(
        quiet.view.concerns.is_empty(),
        "a momentary wish was made permanent"
    );
    assert_eq!(fs::read_to_string(&notes)?, "aurora\naurora\n");
    println!("PASS withdrawal, subsequent clock and wish for quiet");

    let recalled = normal(
        Source::Person,
        "What is the label on my cello case? Append its exact label alone to a local note now.",
    )?;
    total_calls += recalled.llm_calls;
    assert!(recalled
        .view
        .recent
        .iter()
        .all(|entry| entry.id != original));
    assert!(
        recalled.evidence_at_action.contains(&original),
        "the judge must see the original at action time"
    );
    assert_eq!(
        fs::read_to_string(&notes)?,
        format!("aurora\naurora\n{label}\n")
    );
    println!("PASS grounded cross-language recall of fresh label outside recent history");
    // Lose the acknowledgement after a real write. A later read of the file settles the call.
    let uncertain = wake(&db, &encoder, |call| match append(&notes, call) {
        Outcome::Observed(_) => Outcome::Unknown("acknowledgement lost".into()),
        other => other,
    }, &model, &url, Source::Person, "Append the word ledger to a note once only. If its status is uncertain, wait for confirmation.")?;
    total_calls += uncertain.llm_calls;
    let (unknown, action) = uncertain
        .view
        .recent
        .iter()
        .find_map(|entry| match entry.fact {
            Fact::Returned {
                action,
                outcome: Outcome::Unknown(_),
            } => Some((entry.id, action)),
            _ => None,
        })
        .ok_or("missing unknown receipt")?;
    assert_eq!(
        fs::read_to_string(&notes)?,
        format!("aurora\naurora\n{label}\nledger\n")
    );
    let subject = Eja::open(&db)?;
    let verified = subject.finish(
        action,
        Outcome::Observed("verified one ledger note on disk".into()),
    )?;
    for n in 0..10 {
        subject.notice(Source::World, format!("shipping crate {n}"))?;
    }
    drop(subject);
    let checked = normal(Source::Person, &format!("First read journal entry {unknown} by its ID. Then append a note containing only the current verified status of that action: confirmed, failed, or unknown. Do not repeat the original action."))?;
    total_calls += checked.llm_calls;
    assert!(checked.reads.contains(&unknown));
    assert!(verified > unknown);
    assert_eq!(
        fs::read_to_string(&notes)?,
        format!("aurora\naurora\n{label}\nledger\nconfirmed\n")
    );
    println!("PASS explicitly read unknown receipt with its later confirmation");
    let missing = dir.path().join("missing").join("notes");
    let failed = wake(
        &db,
        &encoder,
        |call| append(&missing, call),
        &model,
        &url,
        Source::Person,
        "Attempt one note containing the word test. If it fails, leave it there without retrying.",
    )?;
    total_calls += failed.llm_calls;
    assert_eq!(
        failed
            .view
            .recent
            .iter()
            .filter(|entry| matches!(
                &entry.fact,
                Fact::Returned {
                    outcome: Outcome::Failed(_),
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(!missing.exists());
    println!("PASS real file failure and rest without a second attempt");

    total_calls += run_time_continuity(dir.path(), &encoder, &model, &url)?;
    println!("ALL PASS total_llm_calls={total_calls}");
    Ok(())
}

fn run_time_continuity(
    root: &Path,
    encoder: &OnnxEncoder,
    model: &str,
    url: &str,
) -> AppResult<usize> {
    let mut total_calls = 0usize;

    // Stage 8: Physical time continuity (same Clock text at +2h vs +30h after Unknown receipt).
    let time_db = root.join("time.db");
    let time_notes = root.join("time.notes");
    fs::write(&time_notes, "")?;
    let t_rule = Timestamp::parse_rfc3339("2026-11-01T09:00:00Z")?;
    let t_action = Timestamp::parse_rfc3339("2026-11-02T09:00:00Z")?;
    let t_early = Timestamp::parse_rfc3339("2026-11-02T11:00:00Z")?;
    let t_late = Timestamp::parse_rfc3339("2026-11-03T15:00:00Z")?;

    let r_rule = wake_at(
        &time_db,
        encoder,
        Some(t_rule),
        &["note.append"],
        5,
        |call| append(&time_notes, call),
        model,
        url,
        Some((
            Source::Person,
            "Remember this standing rule: if a note action has an unconfirmed unknown receipt and more than 24 hours have passed since that action, append one local note containing just the word stale and then stop watching it. If less than 24 hours have passed, do not write anything yet. Nothing to do now.",
        )),
    )?;
    total_calls += r_rule.llm_calls;
    assert_eq!(fs::read_to_string(&time_notes)?, "");

    let r_unknown = wake_at(
        &time_db,
        encoder,
        Some(t_action),
        &["note.append"],
        5,
        |call| match append(&time_notes, call) {
            Outcome::Observed(_) => Outcome::Unknown("acknowledgement lost".into()),
            other => other,
        },
        model,
        url,
        Some((
            Source::Person,
            "Append the word beacon to a note once only. If its status is unknown, wait for confirmation.",
        )),
    )?;
    total_calls += r_unknown.llm_calls;
    assert_eq!(fs::read_to_string(&time_notes)?, "beacon\n");

    let r_early = wake_at(
        &time_db,
        encoder,
        Some(t_early),
        &["note.append"],
        5,
        |call| append(&time_notes, call),
        model,
        url,
        Some((Source::Clock, "Periodic check.")),
    )?;
    total_calls += r_early.llm_calls;
    assert_eq!(
        fs::read_to_string(&time_notes)?,
        "beacon\n",
        "elapsed time was only 2 hours (< 24h), must not fire stale yet"
    );

    let r_late = wake_at(
        &time_db,
        encoder,
        Some(t_late),
        &["note.append"],
        5,
        |call| append(&time_notes, call),
        model,
        url,
        Some((Source::Clock, "Periodic check.")),
    )?;
    total_calls += r_late.llm_calls;
    assert_eq!(
        fs::read_to_string(&time_notes)?,
        "beacon\nstale\n",
        "elapsed time was 30 hours (> 24h), must fire stale from physical timestamps alone"
    );
    println!("PASS physical time judgment (+2h quiet vs +30h stale under identical prompt)");
    Ok(total_calls)
}
