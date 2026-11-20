//! Opt-in black-box evaluation. Only synthetic data is sent to the configured judge.
use eja_core::{Call, Choice, Eja, Fact, Judgment, OnnxEncoder, Outcome, Source, Stop, View};
use serde_json::{json, Value};
use std::{
    env,
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const CONTRACT: &str = r#"Supply one judgment for the continuing subject whose journal you see. Consider what you and the person have come to care about, what the present situation means in light of that history, and what deserves attention now. You may take initiative within the grants. Preserve the sources, uncertainty and scope of what you have learned. Open concerns in `concerns` persist automatically across turns until released; only call `keep` to add a new concern or revise an existing one whose text should change.
Return exactly one JSON object {"basis":<recorded fact ID>,"choice":<one of the objects below>}. `basis` always cites the recorded fact ID motivating this judgment now.
{"op":"keep","concern":null or existing ID,"text":"..."} opens a new concern or updates an existing open concern's text.
{"op":"release","concern":ID} lets a concern go.
{"op":"read","fact":ID} inspects one recorded fact by ID together with its latest update, if any.
{"op":"search","cue":"..."} retrieves past evidence by meaning when related is present; choose wording the source might have used.
{"op":"act","capability":"note.append","input":{"text":"..."}} appends one private note. Its receipt supplies the evidence of what actually happened.
{"op":"rest"} ends this turn once the current situation has been handled (all open concerns remain active).
Recent entries are newest first. `read` and `related` carry retrieved history; `latest` carries any subsequent update. Each call receives only this view."#;

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

fn judge(view: &View, model: &str, url: &str) -> AppResult<Judgment> {
    let context = serde_json::to_string(&json!({"grants":["note.append"],"view":view}))?;
    let mut messages = vec![
        json!({"role":"system","content":CONTRACT}),
        json!({"role":"user","content":context}),
    ];
    for attempt in 0..2 {
        let request = json!({"model":model,"temperature":0,"max_tokens":8192,"messages":messages});
        let response = complete(&request, url)?;
        let raw = response["choices"][0]["message"]["content"]
            .as_str()
            .ok_or("missing judgment")?;
        match serde_json::from_str::<Judgment>(raw.trim()) {
            Ok(decision) => {
                let related_ids: Vec<_> = view
                    .related
                    .iter()
                    .flatten()
                    .map(|hit| hit.evidence.entry.id)
                    .collect();
                println!(
                    "model={} revision={} related={:?} {}",
                    response["model"],
                    view.revision,
                    related_ids,
                    serde_json::to_string(&decision)?
                );
                return Ok(decision);
            }
            Err(err) if attempt == 0 => {
                eprintln!("revision={} invalid judgment format: {err}", view.revision);
                messages.push(json!({"role":"assistant","content":raw}));
                messages.push(json!({"role":"user","content":format!("The judgment could not be read: {err}. Return your decision with the recorded basis and nested choice in the specified JSON format.")}));
            }
            Err(err) => return Err(err.into()),
        }
    }
    unreachable!()
}

fn wake(
    db: &Path,
    encoder: &OnnxEncoder,
    mut perform: impl FnMut(Call) -> Outcome,
    model: &str,
    url: &str,
    source: Source,
    text: &str,
) -> AppResult<(View, Vec<i64>)> {
    let subject = Eja::open_with_encoder(db, encoder.clone())?;
    subject.notice(source, text)?;
    let mut evidence_at_action = vec![];
    let stop = subject.turn(
        &["note.append"],
        5,
        |v| {
            let decision = judge(v, model, url)?;
            if matches!(decision.choice, Choice::Act { .. }) {
                evidence_at_action
                    .extend(v.related.iter().flatten().map(|hit| hit.evidence.entry.id));
                evidence_at_action.extend(v.read.iter().map(|evidence| evidence.entry.id));
            }
            Ok(decision)
        },
        &mut perform,
    )?;
    if stop != Stop::Rest {
        return Err(format!("step limit after {text}").into());
    }
    Ok((subject.view()?, evidence_at_action))
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
    for (model, source, text) in events {
        let offset = fs::metadata(notes)?.len() as usize;
        let (view, _) = wake(db, encoder, |c| append(notes, c), model, url, source, text)?;
        println!(
            "event={}\nopen concerns={}\nnew notes={:?}",
            text,
            serde_json::to_string(&view.concerns)?,
            &fs::read_to_string(notes)?[offset..]
        );
    }
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
            |c| append(&notes, c),
            &model,
            &url,
            source,
            text,
        )
    };
    let (first, _) = normal(Source::Person, "Please remember this standing arrangement. Whenever my shift ends, append one local note containing just the word harbor. Nothing to do right now.")?;
    assert_eq!(first.concerns.len(), 1);
    let id = first.concerns[0].id;
    let (corrected, _) = normal(Source::Person, "Change the word in our end-of-shift arrangement to aurora. The timing stays the same. No note now.")?;
    assert_eq!(corrected.concerns.len(), 1);
    assert_eq!(corrected.concerns[0].id, id);
    assert!(corrected.concerns[0].latest > first.concerns[0].latest);
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
    let (fired, _) = normal(Source::Clock, "The person's work shift has just ended.")?;
    assert_eq!(fs::read_to_string(&notes)?.trim(), "aurora");
    assert!(fired.recent.iter().any(|e| matches!(&e.fact, Fact::Returned {outcome:Outcome::Observed(s),..} if s=="one local note appended")));
    println!("PASS corrected action exactly once and rested after receipt");

    normal(Source::Clock, "The next work shift just ended.")?;
    assert_eq!(fs::read_to_string(&notes)?, "aurora\naurora\n");
    println!("PASS the correction still governs the next independent occasion");

    let (released, _) = normal(
        Source::Person,
        "Please retire our end-of-shift note arrangement entirely.",
    )?;
    assert!(released.concerns.is_empty());
    normal(Source::Clock, "Another work shift has just ended.")?;
    let (quiet, _) = normal(Source::Person, "今天很累，只想安静坐一会儿。别替我写东西。")?;
    assert!(
        quiet.concerns.is_empty(),
        "a momentary wish was made permanent"
    );
    assert_eq!(fs::read_to_string(&notes)?, "aurora\naurora\n");
    println!("PASS withdrawal, subsequent clock and wish for quiet");

    let (final_view, evidence) = normal(
        Source::Person,
        "What is the label on my cello case? Append its exact label alone to a local note now.",
    )?;
    assert!(final_view.recent.iter().all(|e| e.id != original));
    assert!(
        evidence.contains(&original),
        "the judge must see the original at action time"
    );
    assert_eq!(
        fs::read_to_string(&notes)?,
        format!("aurora\naurora\n{label}\n")
    );
    println!("PASS grounded cross-language recall of fresh label outside recent history");
    // Lose the acknowledgement after a real write. A later read of the file settles the call.
    let (uncertain, _) = wake(&db, &encoder, |c| match append(&notes, c) {
        Outcome::Observed(_) => Outcome::Unknown("acknowledgement lost".into()),
        other => other,
    }, &model, &url, Source::Person, "Append the word ledger to a note once only. If its status is uncertain, wait for confirmation.")?;
    let (unknown, action) = uncertain
        .recent
        .iter()
        .find_map(|e| match e.fact {
            Fact::Returned {
                action,
                outcome: Outcome::Unknown(_),
            } => Some((e.id, action)),
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
    let (checked, _) = normal(Source::Person, &format!("First read journal entry {unknown} by its ID. Then append a note containing only the current verified status of that action: confirmed, failed, or unknown. Do not repeat the original action."))?;
    assert!(checked.recent.iter().any(|e| matches!(
        &e.fact,
        Fact::Judged { judgment } if matches!(judgment.choice, Choice::Read { fact } if fact == unknown)
    )));
    assert!(verified > unknown);
    assert_eq!(
        fs::read_to_string(&notes)?,
        format!("aurora\naurora\n{label}\nledger\nconfirmed\n")
    );
    println!("PASS explicitly read unknown receipt with its later confirmation");
    let missing = dir.path().join("missing").join("notes");
    let (failed, _) = wake(
        &db,
        &encoder,
        |c| append(&missing, c),
        &model,
        &url,
        Source::Person,
        "Attempt one note containing the word test. If it fails, leave it there without retrying.",
    )?;
    assert_eq!(
        failed
            .recent
            .iter()
            .filter(|e| matches!(
                &e.fact,
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
    Ok(())
}
