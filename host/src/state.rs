//! Host-owned scheduling, turn settlement, window anchor, and upgrade state stored
//! atomically alongside `eja-core` facts via `host_meta`.
use crate::sandbox::TrustedDir;
use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Source, Tx, FACT_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

const META_STATE: &str = "host.state";
const MAX_PROACTIVE_HISTORY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeAlarm {
    pub id: String,
    pub fire_at_unix: i64,
    pub fire_at_display: String,
    pub reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostState {
    #[serde(default)]
    turn_cursor: Option<Id>,
    #[serde(default)]
    window_anchor: Option<Id>,
    #[serde(default)]
    wakes: Vec<WakeAlarm>,
    #[serde(default)]
    kernel_version: Option<String>,
    #[serde(default)]
    proactive_replies: Vec<Id>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyWindowFile {
    #[serde(default)]
    anchor: Id,
    #[serde(default)]
    settled_through: Option<Id>,
}

fn decode_state(raw: Option<String>) -> eja_core::Result<HostState> {
    raw.map(|json| serde_json::from_str(&json))
        .transpose()?
        .map_or_else(|| Ok(HostState::default()), Ok)
}

fn update_state<T>(
    eja: &Eja,
    f: impl FnOnce(&Tx<'_>, &mut HostState) -> eja_core::Result<T>,
) -> eja_core::Result<T> {
    eja.transaction(|tx| {
        let mut state = decode_state(tx.get_meta(META_STATE)?)?;
        let out = f(tx, &mut state)?;
        tx.set_meta(META_STATE, &serde_json::to_string(&state)?)?;
        Ok(out)
    })
}

fn find_trigger(entries: &[Entry]) -> Option<(Id, Source)> {
    entries
        .iter()
        .find_map(|entry| trigger_source(&entry.fact).map(|source| (entry.id, source)))
}

fn wake_notice_text(reason: &str) -> String {
    format!("定时唤醒：{reason}")
}

pub fn trigger_source(fact: &Fact) -> Option<Source> {
    match fact {
        Fact::Noticed {
            source: source @ (Source::Person | Source::Clock),
            ..
        } => Some(source.clone()),
        _ => None,
    }
}

pub fn entries_for_turn(
    eja: &Eja,
    after: Id,
    through: Id,
    trigger: Id,
) -> eja_core::Result<Vec<Entry>> {
    let mut entries = eja.entries_after(after, through)?;
    entries.retain(|entry| entry.id <= trigger || trigger_source(&entry.fact).is_none());
    Ok(entries)
}

pub fn bootstrap(
    eja: &Eja,
    core_dir: &TrustedDir,
    resources_dir: Option<&Path>,
    kernel_version: &str,
    protocol_version: u32,
) -> Result<Id, String> {
    let legacy_wakes_path = core_dir.path().join("wakes.json");
    let legacy_wakes: Option<Vec<WakeAlarm>> = match fs::read(&legacy_wakes_path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|err| err.to_string())?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.to_string()),
    };
    let legacy_window: Option<LegacyWindowFile> = core_dir
        .read_file("window.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LegacyWindowFile>(&bytes).ok());
    let legacy_version: Option<String> = core_dir
        .read_file("kernel_version")
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let anchor = update_state(eja, |tx, state| {
        if let Some(alarms) = &legacy_wakes {
            merge_wakes(&mut state.wakes, alarms)?;
        }

        let unfinished = tx.unfinished_actions()?;
        if state.turn_cursor.is_none() {
            let legacy_settled = legacy_window
                .as_ref()
                .and_then(|w| w.settled_through)
                .map(|id| id.max(0));
            state.turn_cursor = Some(compute_initial_cursor(
                tx,
                legacy_settled,
                unfinished.first().copied(),
            )?);
        }

        let anchor = *state
            .window_anchor
            .get_or_insert_with(|| legacy_window.as_ref().map_or(0, |w| w.anchor.max(0)));
        for action in unfinished {
            tx.finish(
                action,
                Outcome::Unknown("上次执行因宿主中断而未完成".into()),
            )?;
        }

        if let Some(old) = state
            .kernel_version
            .as_deref()
            .or(legacy_version.as_deref())
            .filter(|old| *old != kernel_version)
        {
            let suffix = resources_dir
                .map(|dir| {
                    format!(
                        "协议文档与默认界面参考位于 {}，可按需对照调整本地前端或连接器。",
                        dir.display()
                    )
                })
                .unwrap_or_default();
            tx.notice(
                Source::World,
                format!(
                    "内核已从 {old} 升级到 {kernel_version}（协议 v{protocol_version}）。{suffix}"
                ),
            )?;
        }
        state.kernel_version = Some(kernel_version.to_string());
        Ok(anchor.max(0))
    })
    .map_err(|err| err.to_string())?;

    for (migrated, name) in [
        (legacy_wakes.is_some(), "wakes.json"),
        (legacy_window.is_some(), "window.json"),
        (legacy_version.is_some(), "kernel_version"),
    ] {
        if migrated {
            let _ = fs::remove_file(core_dir.path().join(name));
        }
    }
    Ok(anchor)
}

fn compute_initial_cursor(
    tx: &Tx<'_>,
    legacy: Option<Id>,
    first_unfinished: Option<Id>,
) -> eja_core::Result<Id> {
    let head = tx.revision()?;
    let all = tx.entries_after(0, head)?;
    let through = if let Some(id) = legacy.filter(|id| (0..=head).contains(id)) {
        id
    } else if let Some(action) = first_unfinished {
        all.iter()
            .rev()
            .find(|entry| entry.id < action && trigger_source(&entry.fact).is_some())
            .map_or(head, |entry| entry.id - 1)
    } else {
        all.iter()
            .rev()
            .find(|entry| !matches!(entry.fact, Fact::Noticed { .. }))
            .map_or(0, |entry| entry.id)
    };
    let after_said = all
        .iter()
        .rev()
        .find(|entry| entry.id > through && matches!(entry.fact, Fact::Said { .. }))
        .map(|entry| entry.id);
    Ok(after_said.unwrap_or(through))
}

pub fn save_anchor(eja: &Eja, anchor: Id) -> Result<(), String> {
    update_state(eja, |_, state| {
        state.window_anchor = Some(anchor.max(0));
        Ok(())
    })
    .map_err(|err| err.to_string())
}

pub fn next_trigger(eja: &Eja) -> eja_core::Result<Option<(Id, Source)>> {
    let cursor = decode_state(eja.get_meta(META_STATE)?)?
        .turn_cursor
        .unwrap_or(0);
    let head = eja.revision()?;
    Ok(find_trigger(&eja.entries_after(cursor, head)?))
}

/// Atomically record an optional reply and advance the host turn cursor for `trigger`.
pub fn complete_turn(eja: &Eja, trigger: Id, reply: &str) -> eja_core::Result<()> {
    update_state(eja, |tx, state| {
        let cursor = state.turn_cursor.unwrap_or(0);
        let head = tx.revision()?;
        let Some((expected, source)) = find_trigger(&tx.entries_after(cursor, head)?) else {
            return Err(Error::Conflict);
        };
        if expected != trigger {
            return Err(Error::Conflict);
        }
        if !reply.trim().is_empty() {
            let said_id = tx.say(reply)?;
            if source == Source::Clock {
                state.proactive_replies.push(said_id);
                if state.proactive_replies.len() > MAX_PROACTIVE_HISTORY {
                    let excess = state.proactive_replies.len() - MAX_PROACTIVE_HISTORY;
                    state.proactive_replies.drain(0..excess);
                }
            }
        }
        state.turn_cursor = Some(trigger);
        Ok(())
    })
}

pub fn proactive_reply_ids(eja: &Eja) -> HashSet<Id> {
    decode_state(eja.get_meta(META_STATE).ok().flatten())
        .ok()
        .map(|state| state.proactive_replies.into_iter().collect())
        .unwrap_or_default()
}

fn merge_wakes(existing: &mut Vec<WakeAlarm>, alarms: &[WakeAlarm]) -> eja_core::Result<()> {
    for alarm in alarms {
        let preview = Fact::Noticed {
            source: Source::Clock,
            text: wake_notice_text(&alarm.reason),
        };
        if serde_json::to_vec(&preview)?.len() > FACT_BYTES {
            return Err(Error::Invalid("wake notice exceeds the byte budget".into()));
        }
    }
    for alarm in alarms {
        if !existing.iter().any(|item| item.id == alarm.id) {
            existing.push(alarm.clone());
        }
    }
    existing.sort_by(|a, b| a.fire_at_unix.cmp(&b.fire_at_unix).then(a.id.cmp(&b.id)));
    Ok(())
}

pub fn schedule_wakes(eja: &Eja, alarms: &[WakeAlarm]) -> eja_core::Result<()> {
    update_state(eja, |_, state| merge_wakes(&mut state.wakes, alarms))
}

pub fn pending_wakes(eja: &Eja) -> eja_core::Result<Vec<WakeAlarm>> {
    Ok(decode_state(eja.get_meta(META_STATE)?)?.wakes)
}

pub fn next_wake_at(eja: &Eja) -> eja_core::Result<Option<i64>> {
    Ok(pending_wakes(eja)?.first().map(|alarm| alarm.fire_at_unix))
}

/// Atomically record all wakes due at or before `eja.now()` and remove them from the queue.
pub fn record_due_wakes(eja: &Eja) -> eja_core::Result<bool> {
    eja.transaction(|tx| {
        let mut state = decode_state(tx.get_meta(META_STATE)?)?;
        let now = tx.now()?.as_unix();
        if !state.wakes.iter().any(|alarm| alarm.fire_at_unix <= now) {
            return Ok(false);
        }
        let (due, remaining): (Vec<_>, Vec<_>) = state
            .wakes
            .drain(..)
            .partition(|alarm| alarm.fire_at_unix <= now);
        for alarm in &due {
            tx.notice(Source::Clock, wake_notice_text(&alarm.reason))?;
        }
        state.wakes = remaining;
        tx.set_meta(META_STATE, &serde_json::to_string(&state)?)?;
        Ok(true)
    })
}

#[cfg(test)]
pub fn set_kernel_version_for_test(eja: &Eja, version: &str) -> eja_core::Result<()> {
    update_state(eja, |_, state| {
        state.kernel_version = Some(version.to_string());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eja_core::Timestamp;

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

    #[test]
    fn record_due_wakes_uses_subject_clock_and_removes_fired_alarms() {
        let eja = Eja::open(":memory:").unwrap().with_clock(|| Timestamp(100));
        schedule_wakes(
            &eja,
            &[
                WakeAlarm {
                    id: "w-1".into(),
                    fire_at_unix: 100,
                    fire_at_display: "00:01:40".into(),
                    reason: "due now".into(),
                },
                WakeAlarm {
                    id: "w-2".into(),
                    fire_at_unix: 200,
                    fire_at_display: "00:03:20".into(),
                    reason: "later".into(),
                },
            ],
        )
        .unwrap();
        assert!(record_due_wakes(&eja).unwrap());
        let remaining = pending_wakes(&eja).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "w-2");
        assert!(matches!(
            eja.read(1).unwrap().fact,
            Fact::Noticed {
                source: Source::Clock,
                ref text,
            } if text == "定时唤醒：due now"
        ));
        assert!(!record_due_wakes(&eja).unwrap());
    }
}
