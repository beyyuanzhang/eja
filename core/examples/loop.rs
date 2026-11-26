//! Offline application: a scripted judge and one host-owned file capability.
use eja_core::{Call, eja, Fact, Outcome, Source};
use serde_json::Value;
use std::{error::Error, fs, path::Path};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const REQUEST: &str = "下次复看时，帮我看看草稿有几行";

fn step(eja: &eja, draft: &Path, grants: &[&str]) -> AppResult<()> {
    let head = eja.revision()?;
    let concerns = eja.concerns(head)?;
    let recent = eja.recent(1)?;
    let newest = recent.last().ok_or("expected an observation")?;
    match &newest.fact {
        Fact::Noticed {
            source: Source::Person,
            text,
        } => {
            eja.keep(concerns.first().map(|c| c.id), text)?;
        }
        Fact::Noticed {
            source: Source::Clock,
            ..
        } if concerns.first().is_some_and(|c| c.text == REQUEST) => {
            if !grants.contains(&"draft.lines") {
                return Ok(());
            }
            let call = eja.act(head, "draft.lines", Value::Null)?;
            let outcome = perform(&call, draft);
            eja.finish(call.id, outcome)?;
        }
        _ => {}
    }
    Ok(())
}

fn perform(call: &Call, draft: &Path) -> Outcome {
    match call.capability.as_str() {
        "draft.lines" => match fs::read_to_string(draft) {
            Ok(text) => Outcome::Observed(format!("{} lines", text.lines().count())),
            Err(err) => Outcome::Failed(err.to_string()),
        },
        _ => Outcome::Failed("unknown capability".into()),
    }
}

fn main() -> AppResult<()> {
    let dir = tempfile::tempdir()?;
    let journal = dir.path().join("subject.db");
    let draft = dir.path().join("draft.txt");
    fs::write(&draft, "first line\nsecond line\n")?;
    let eja = eja::open(&journal)?;
    eja.notice(Source::Person, REQUEST)?;
    step(&eja, &draft, &[])?;
    drop(eja);

    let eja = eja::open(&journal)?;
    eja.notice(Source::Clock, "到了约定的复看时间")?;
    step(&eja, &draft, &["draft.lines"])?;
    let recent = eja.recent(8)?;
    let receipt = recent
        .iter()
        .find(|entry| {
            matches!(&entry.fact,
                Fact::Returned { outcome: Outcome::Observed(text), .. } if text == "2 lines"
            )
        })
        .ok_or("missing host observation")?;
    println!("{}", serde_json::to_string(receipt)?);

    eja.notice(Source::Person, "以后等我问起再看")?;
    step(&eja, &draft, &[])?;
    eja.notice(Source::Clock, "再次复看")?;
    let rev_before = eja.revision()?;
    step(&eja, &draft, &["draft.lines"])?;
    assert_eq!(eja.revision()?, rev_before);
    let concerns = eja.concerns(eja.revision()?)?;
    println!("{}", concerns[0].text);
    Ok(())
}
