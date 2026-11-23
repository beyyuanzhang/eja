//! Host-owned scheduling, turn settlement, window anchor, and upgrade state stored
//! atomically alongside `eja-core` facts via `host_meta`.
use crate::http::truncate_utf8;
use crate::sandbox::TrustedDir;
use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Source, Tx, FACT_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

const META_STATE: &str = "host.state";
const MAX_PROACTIVE_HISTORY: usize = 256;

fn default_guard_interval() -> i64 {
    60
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_probe_input() -> Value {
    serde_json::json!({})
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConcernCheckMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_criteria: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_fact_id: Option<Id>,
    #[serde(default)]
    pub require_witness: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_witness_fact_id: Option<Id>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardTrigger {
    #[default]
    Changed,
    ExitZero,
    NonEmpty,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuardProbe {
    Command {
        command: String,
    },
    Tool {
        capability: String,
        #[serde(default = "default_probe_input")]
        input: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeGuard {
    pub probe: GuardProbe,
    #[serde(default = "default_guard_interval")]
    pub interval_seconds: i64,
    #[serde(default)]
    pub trigger_on: GuardTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_checks: Option<u32>,
    #[serde(default)]
    pub checks_done: u32,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeAlarm {
    pub id: String,
    pub fire_at_unix: i64,
    pub fire_at_display: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<WakeGuard>,
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
    #[serde(default)]
    world_triggers: Vec<Id>,
    #[serde(default)]
    concern_checks: BTreeMap<Id, ConcernCheckMeta>,
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

fn is_trigger_entry(entry: &Entry, world_triggers: &[Id]) -> Option<Source> {
    match &entry.fact {
        Fact::Noticed {
            source: source @ (Source::Person | Source::Clock),
            ..
        } => Some(source.clone()),
        Fact::Noticed {
            source: Source::World,
            ..
        } if world_triggers.contains(&entry.id) => Some(Source::World),
        _ => None,
    }
}

fn find_trigger(entries: &[Entry], world_triggers: &[Id]) -> Option<(Id, Source)> {
    entries
        .iter()
        .find_map(|entry| is_trigger_entry(entry, world_triggers).map(|source| (entry.id, source)))
}

fn wake_notice_text(reason: &str) -> String {
    format!("定时唤醒：{reason}")
}

pub fn entries_for_turn(
    eja: &Eja,
    after: Id,
    through: Id,
    trigger: Id,
) -> eja_core::Result<Vec<Entry>> {
    let world_triggers = decode_state(eja.get_meta(META_STATE)?)?.world_triggers;
    let mut entries = eja.entries_after(after, through)?;
    entries.retain(|entry| entry.id <= trigger || is_trigger_entry(entry, &world_triggers).is_none());
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
                &state.world_triggers,
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
    world_triggers: &[Id],
) -> eja_core::Result<Id> {
    let head = tx.revision()?;
    let all = tx.entries_after(0, head)?;
    let through = if let Some(id) = legacy.filter(|id| (0..=head).contains(id)) {
        id
    } else if let Some(action) = first_unfinished {
        all.iter()
            .rev()
            .find(|entry| entry.id < action && is_trigger_entry(entry, world_triggers).is_some())
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
    let state = decode_state(eja.get_meta(META_STATE)?)?;
    let cursor = state.turn_cursor.unwrap_or(0);
    let head = eja.revision()?;
    Ok(find_trigger(
        &eja.entries_after(cursor, head)?,
        &state.world_triggers,
    ))
}

/// Atomically record an optional reply and advance the host turn cursor for `trigger`.
pub fn complete_turn(eja: &Eja, trigger: Id, reply: &str) -> eja_core::Result<()> {
    update_state(eja, |tx, state| {
        let cursor = state.turn_cursor.unwrap_or(0);
        let head = tx.revision()?;
        let Some((expected, source)) =
            find_trigger(&tx.entries_after(cursor, head)?, &state.world_triggers)
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
        state.turn_cursor = Some(trigger);
        state.world_triggers.retain(|&id| id > trigger);
        Ok(())
    })
}

pub fn proactive_reply_ids(eja: &Eja) -> HashSet<Id> {
    decode_state(eja.get_meta(META_STATE).ok().flatten())
        .ok()
        .map(|state| state.proactive_replies.into_iter().collect())
        .unwrap_or_default()
}

pub fn concern_check_metas(eja: &Eja) -> BTreeMap<Id, ConcernCheckMeta> {
    decode_state(eja.get_meta(META_STATE).ok().flatten())
        .ok()
        .map(|state| state.concern_checks)
        .unwrap_or_default()
}

/// Atomically keep/revise a concern together with optional evidence/verification metadata.
pub fn keep_with_check(
    eja: &Eja,
    concern: Option<Id>,
    text: &str,
    evidence_fact_id: Option<Id>,
    verify_criteria: Option<String>,
    require_witness: Option<bool>,
) -> eja_core::Result<(Entry, ConcernCheckMeta)> {
    update_state(eja, |tx, state| {
        if let Some(ev_id) = evidence_fact_id {
            tx.read(ev_id)
                .map_err(|_| Error::Invalid(format!("evidence_fact_id {ev_id} 不存在")))?;
        }
        let entry = tx.keep(concern, text)?;
        let Fact::Kept { concern: cid, .. } = entry.fact else {
            unreachable!()
        };
        let meta = state.concern_checks.entry(cid).or_default();
        if let Some(ev_id) = evidence_fact_id {
            meta.evidence_fact_id = Some(ev_id);
        }
        if let Some(criteria) = verify_criteria
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            meta.verify_criteria = Some(criteria);
            if require_witness.is_none() {
                meta.require_witness = true;
            }
        }
        if let Some(req) = require_witness {
            meta.require_witness = req;
        }
        Ok((entry, meta.clone()))
    })
}

fn validate_witness(tx: &Tx<'_>, concern: Id, witness_id: Id) -> eja_core::Result<()> {
    if witness_id < concern {
        return Err(Error::Invalid(format!(
            "witness_fact_id ({witness_id}) 早于挂念创建记录 ({concern})，不能作为后续验收证据"
        )));
    }
    let ev = tx
        .evidence(witness_id)
        .map_err(|_| Error::Invalid(format!("witness_fact_id {witness_id} 不存在")))?;
    let effective = ev.latest.as_ref().unwrap_or(&ev.entry);
    match &effective.fact {
        Fact::Returned {
            outcome: Outcome::Observed(_),
            ..
        } => Ok(()),
        Fact::Noticed {
            source: Source::Person | Source::World,
            ..
        } => Ok(()),
        Fact::Acted { .. } => Err(Error::Invalid(format!(
            "fact {witness_id} 是尚未完成回执的动作，不能作为验收证据"
        ))),
        Fact::Returned {
            outcome: Outcome::Failed(_),
            ..
        } => Err(Error::Invalid(format!(
            "fact {witness_id} 的回执为 failed，不能作为完成验收证据"
        ))),
        Fact::Returned {
            outcome: Outcome::Unknown(_),
            ..
        } => Err(Error::Invalid(format!(
            "fact {witness_id} 的回执为 unknown（未证实），不能作为完成验收证据"
        ))),
        Fact::Said { .. } => Err(Error::Invalid(format!(
            "fact {witness_id} 是主体自身发言（said），自述不能替代客观验收证据"
        ))),
        Fact::Noticed {
            source: Source::Clock,
            ..
        }
        | Fact::Kept { .. }
        | Fact::Released { .. } => Err(Error::Invalid(format!(
            "fact {witness_id} 不是客观实测（observed）或外部/人工确认，不能作为验收证据"
        ))),
    }
}

/// Atomically verify witness (when required or supplied) and release an open concern.
pub fn release_with_witness(
    eja: &Eja,
    concern: Id,
    witness_fact_id: Option<Id>,
) -> eja_core::Result<Entry> {
    update_state(eja, |tx, state| {
        let requires = state
            .concern_checks
            .get(&concern)
            .is_some_and(|meta| meta.require_witness);
        match witness_fact_id {
            Some(wid) => validate_witness(tx, concern, wid)?,
            None if requires => {
                return Err(Error::Invalid(format!(
                    "挂念 {concern} 绑定了验收约束，release 必须提供指向实测 observed 或用户/世界确认的 witness_fact_id"
                )));
            }
            None => {}
        }
        let entry = tx.release(concern)?;
        if let Some(wid) = witness_fact_id {
            state
                .concern_checks
                .entry(concern)
                .or_default()
                .release_witness_fact_id = Some(wid);
        }
        Ok(entry)
    })
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
#[cfg(test)]
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

/// Atomically record due unguarded wakes and return due guarded wakes for out-of-tx probe evaluation.
pub fn collect_due_wakes(eja: &Eja) -> eja_core::Result<(bool, Vec<WakeAlarm>, i64)> {
    eja.transaction(|tx| {
        let mut state = decode_state(tx.get_meta(META_STATE)?)?;
        let now = tx.now()?.as_unix();
        let mut fired_plain = false;
        let mut due_guarded = Vec::new();
        let mut remaining = Vec::with_capacity(state.wakes.len());
        for alarm in state.wakes.drain(..) {
            if alarm.fire_at_unix <= now {
                if alarm.guard.is_some() {
                    due_guarded.push(alarm.clone());
                    remaining.push(alarm);
                } else {
                    tx.notice(Source::Clock, wake_notice_text(&alarm.reason))?;
                    fired_plain = true;
                }
            } else {
                remaining.push(alarm);
            }
        }
        state.wakes = remaining;
        if fired_plain {
            tx.set_meta(META_STATE, &serde_json::to_string(&state)?)?;
        }
        Ok((fired_plain, due_guarded, now))
    })
}

pub enum GuardResolution {
    Triggered(String),
    Rearm {
        next_fire_at_unix: i64,
        next_fire_at_display: String,
        guard: WakeGuard,
    },
    Expired,
}

/// Atomically settle one guarded wake after its probe finishes.
/// Rearming or expiring writes zero journal facts.
/// Triggering records a `Source::World` fact and queues its ID as an explicit proactive turn trigger.
pub fn settle_guarded_wake(
    eja: &Eja,
    alarm_id: &str,
    resolution: GuardResolution,
) -> eja_core::Result<bool> {
    eja.transaction(|tx| {
        let mut state = decode_state(tx.get_meta(META_STATE)?)?;
        let Some(pos) = state.wakes.iter().position(|item| item.id == alarm_id) else {
            return Ok(false);
        };
        let fired = match resolution {
            GuardResolution::Triggered(notice) => {
                state.wakes.remove(pos);
                let bounded = truncate_utf8(&notice, FACT_BYTES.saturating_sub(256));
                let fact_id = tx.notice(Source::World, bounded)?;
                if !state.world_triggers.contains(&fact_id) {
                    state.world_triggers.push(fact_id);
                    state.world_triggers.sort_unstable();
                }
                true
            }
            GuardResolution::Rearm {
                next_fire_at_unix,
                next_fire_at_display,
                guard,
            } => {
                let alarm = &mut state.wakes[pos];
                alarm.fire_at_unix = next_fire_at_unix;
                alarm.fire_at_display = next_fire_at_display;
                alarm.guard = Some(guard);
                state
                    .wakes
                    .sort_by(|a, b| a.fire_at_unix.cmp(&b.fire_at_unix).then(a.id.cmp(&b.id)));
                false
            }
            GuardResolution::Expired => {
                state.wakes.remove(pos);
                false
            }
        };
        tx.set_meta(META_STATE, &serde_json::to_string(&state)?)?;
        Ok(fired)
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
                    guard: None,
                },
                WakeAlarm {
                    id: "w-2".into(),
                    fire_at_unix: 200,
                    fire_at_display: "00:03:20".into(),
                    reason: "later".into(),
                    guard: None,
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
