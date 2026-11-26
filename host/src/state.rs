//! Owns turn settlement and window state.
pub mod concern;
pub mod watch;

mod check_evidence;

use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Result, Source, Tx};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

const META_STATE: &str = "host.state";
const MAX_PROACTIVE_HISTORY: usize = 256;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HostState {
    turn_cursor: Id,
    window_anchor: Id,
    kernel_version: Option<String>,
    proactive_replies: Vec<Id>,
}

fn decode_meta<T: DeserializeOwned + Default>(raw: Option<String>) -> Result<T> {
    Ok(raw
        .map(|json| serde_json::from_str(&json))
        .transpose()?
        .unwrap_or_default())
}

fn write_meta(tx: &Tx<'_>, key: &str, value: &impl Serialize) -> Result<()> {
    tx.set_meta(key, &serde_json::to_string(value)?)
}

fn update_state<T>(eja: &Eja, f: impl FnOnce(&Tx<'_>, &mut HostState) -> Result<T>) -> Result<T> {
    eja.transaction(|tx| {
        let mut state = decode_meta(tx.get_meta(META_STATE)?)?;
        let out = f(tx, &mut state)?;
        write_meta(tx, META_STATE, &state)?;
        Ok(out)
    })
}

fn is_trigger_entry(
    entry: &Entry,
    get_meta: &impl Fn(&str) -> Result<Option<String>>,
) -> Result<Option<Source>> {
    Ok(match &entry.fact {
        Fact::Noticed {
            source: source @ (Source::Person | Source::Clock),
            ..
        } => Some(source.clone()),
        Fact::Noticed {
            source: Source::World,
            ..
        } if check_evidence::read_hit(entry.id, get_meta)?.is_some() => Some(Source::World),
        _ => None,
    })
}

fn find_trigger(
    entries: &[Entry],
    get_meta: impl Fn(&str) -> Result<Option<String>>,
) -> Result<Option<(Id, Source)>> {
    for entry in entries {
        if let Some(source) = is_trigger_entry(entry, &get_meta)? {
            return Ok(Some((entry.id, source)));
        }
    }
    Ok(None)
}

pub fn entries_for_turn(eja: &Eja, after: Id, through: Id, trigger: Id) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in eja.entries_after(after, through)? {
        if entry.id <= trigger || is_trigger_entry(&entry, &|key| eja.get_meta(key))?.is_none() {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub fn bootstrap(
    eja: &Eja,
    resources_dir: Option<&Path>,
    kernel_version: &str,
    protocol_version: u32,
) -> std::result::Result<Id, String> {
    eja.transaction(|tx| {
        let stored = tx.get_meta(META_STATE)?;
        if stored.is_none() && tx.revision()? > 0 {
            return Err(Error::Invalid("非空主体日志缺少宿主状态".into()));
        }
        let mut state: HostState = decode_meta(stored)?;
        for action in tx.unfinished_actions()? {
            tx.finish(
                action,
                Outcome::Unknown("上次执行因宿主中断而未完成".into()),
            )?;
        }

        if let Some(old) = state
            .kernel_version
            .as_deref()
            .filter(|old| *old != kernel_version)
        {
            let suffix = resources_dir
                .map(|dir| format!("默认界面与协议文档位于 {}。", dir.display()))
                .unwrap_or_default();
            tx.notice(
                Source::World,
                format!(
                    "内核已从 {old} 升级到 {kernel_version}（协议 v{protocol_version}）。{suffix}"
                ),
            )?;
        }
        state.kernel_version = Some(kernel_version.to_string());
        write_meta(tx, META_STATE, &state)?;
        Ok(state.window_anchor)
    })
    .map_err(|err| err.to_string())
}

pub fn save_anchor(eja: &Eja, anchor: Id) -> std::result::Result<(), String> {
    update_state(eja, |_, state| {
        state.window_anchor = anchor.max(0);
        Ok(())
    })
    .map_err(|err| err.to_string())
}

pub fn next_trigger(eja: &Eja) -> Result<Option<(Id, Source)>> {
    let state: HostState = decode_meta(eja.get_meta(META_STATE)?)?;
    let cursor = state.turn_cursor;
    let head = eja.revision()?;
    find_trigger(&eja.entries_after(cursor, head)?, |key| eja.get_meta(key))
}

/// Atomically record an optional reply and advance the host turn cursor for `trigger`.
pub fn complete_turn(eja: &Eja, trigger: Id, reply: &str) -> Result<()> {
    update_state(eja, |tx, state| {
        let cursor = state.turn_cursor;
        let head = tx.revision()?;
        let Some((expected, source)) =
            find_trigger(&tx.entries_after(cursor, head)?, |key| tx.get_meta(key))?
        else {
            return Err(Error::Conflict);
        };
        if expected != trigger {
            return Err(Error::Conflict);
        }
        if !reply.trim().is_empty() {
            let said_id = tx.say(reply)?;
            if source != Source::Person {
                state.proactive_replies.push(said_id);
                if state.proactive_replies.len() > MAX_PROACTIVE_HISTORY {
                    let excess = state.proactive_replies.len() - MAX_PROACTIVE_HISTORY;
                    state.proactive_replies.drain(0..excess);
                }
            }
        }
        state.turn_cursor = trigger;
        Ok(())
    })
}

pub fn proactive_reply_ids(eja: &Eja) -> HashSet<Id> {
    decode_meta::<HostState>(eja.get_meta(META_STATE).ok().flatten())
        .ok()
        .map(|state| state.proactive_replies.into_iter().collect())
        .unwrap_or_default()
}

#[cfg(test)]
pub fn set_kernel_version_for_test(eja: &Eja, version: &str) -> Result<()> {
    update_state(eja, |_, state| {
        state.kernel_version = Some(version.to_string());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_turn_enforces_order_and_marks_proactive_replies() {
        let eja = Eja::open(":memory:").unwrap();
        let person = eja.notice(Source::Person, "first").unwrap();
        let clock = eja.notice(Source::Clock, "second").unwrap();
        assert!(matches!(
            complete_turn(&eja, clock, "early"),
            Err(Error::Conflict)
        ));
        complete_turn(&eja, person, "reply person").unwrap();
        assert!(matches!(
            complete_turn(&eja, person, "duplicate"),
            Err(Error::Conflict)
        ));
        assert!(proactive_reply_ids(&eja).is_empty());

        complete_turn(&eja, clock, "proactive ping").unwrap();
        let head = eja.revision().unwrap();
        assert!(proactive_reply_ids(&eja).contains(&head));
        assert_eq!(next_trigger(&eja).unwrap(), None);
    }
}
