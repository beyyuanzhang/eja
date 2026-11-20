//! Offline application: a scripted judge and one host-owned file capability.
use eja_core::{Call, Choice, Eja, Fact, Judgment, Outcome, Source, Stop, View};
use serde_json::Value;
use std::{error::Error, fs, path::Path};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const REQUEST: &str = "下次复看时，帮我看看草稿有几行";

fn judge(view: &View) -> AppResult<Judgment> {
    let newest = view.recent.first().ok_or("expected an observation")?;
    let choice = match &newest.fact {
        Fact::Noticed {
            source: Source::Person,
            text,
        } => Choice::Keep {
            concern: view.concerns.first().map(|c| c.id),
            text: text.clone(),
        },
        Fact::Noticed {
            source: Source::Clock,
            ..
        } if view.concerns.first().is_some_and(|c| c.text == REQUEST) => Choice::Act {
            capability: "draft.lines".into(),
            input: Value::Null,
        },
        _ => Choice::Rest,
    };
    Ok(Judgment {
        basis: newest.id,
        choice,
    })
}

fn perform(call: Call, draft: &Path) -> Outcome {
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
    let eja = Eja::open(&journal)?;
    eja.notice(Source::Person, REQUEST)?;
    assert_eq!(eja.turn(&[], 3, judge, |_| unreachable!())?, Stop::Rest);
    drop(eja);

    let eja = Eja::open(&journal)?;
    eja.notice(Source::Clock, "到了约定的复看时间")?;
    let stop = eja.turn(&["draft.lines"], 3, judge, |c| perform(c, &draft))?;
    assert_eq!(stop, Stop::Rest);
    let view = eja.view()?;
    let receipt = view
        .recent
        .iter()
        .find(|e| {
            matches!(&e.fact,
                Fact::Returned { outcome: Outcome::Observed(s), .. } if s == "2 lines"
            )
        })
        .ok_or("missing host observation")?;
    println!("{}", serde_json::to_string(receipt)?);

    eja.notice(Source::Person, "以后等我问起再看")?;
    eja.turn(&[], 3, judge, |_| unreachable!())?;
    eja.notice(Source::Clock, "再次复看")?;
    let stop = eja.turn(&["draft.lines"], 3, judge, |_| {
        panic!("unexpected call after correction")
    })?;
    assert_eq!(stop, Stop::Rest);
    let view = eja.view()?;
    println!("{} → {:?}", view.concerns[0].text, stop);
    Ok(())
}
