//! Host-owned scheduling, turn settlement, and window state stored
//! atomically alongside `eja-core` facts via `host_meta`.
use crate::sandbox::{validate_single_name, TrustedDir};
use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Source, Tx, FACT_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

const META_STATE: &str = "host.state";
const MAX_PROACTIVE_HISTORY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FileCheck {
    file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    not_equals: Option<String>,
}

impl FileCheck {
    pub fn matches(&self, data_dir: &TrustedDir) -> std::io::Result<bool> {
        match data_dir.read_check_file(&self.file, 4096) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(std::io::Error::other)?
                    .trim_end_matches(['\r', '\n']);
                Ok(self
                    .equals
                    .as_deref()
                    .is_none_or(|expected| text == expected)
                    && self
                        .contains
                        .as_deref()
                        .is_none_or(|needle| text.contains(needle))
                    && self
                        .not_equals
                        .as_deref()
                        .is_none_or(|excluded| text != excluded))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WakeGuard {
    pub checker: String,
    pub interval_seconds: i64,
    pub remaining: u32,
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
#[serde(default, deny_unknown_fields)]
struct HostState {
    turn_cursor: Id,
    window_anchor: Id,
    wakes: Vec<WakeAlarm>,
    kernel_version: Option<String>,
    proactive_replies: Vec<Id>,
    world_triggers: Vec<Id>,
    concern_checks: BTreeMap<Id, String>,
    checks: BTreeMap<String, FileCheck>,
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
    entries
        .retain(|entry| entry.id <= trigger || is_trigger_entry(entry, &world_triggers).is_none());
    Ok(entries)
}

pub fn bootstrap(
    eja: &Eja,
    resources_dir: Option<&Path>,
    kernel_version: &str,
    protocol_version: u32,
) -> Result<Id, String> {
    eja.transaction(|tx| {
        let stored = tx.get_meta(META_STATE)?;
        if stored.is_none() && tx.revision()? > 0 {
            return Err(Error::Invalid("非空主体日志缺少宿主状态".into()));
        }
        let mut state = decode_state(stored)?;
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
        tx.set_meta(META_STATE, &serde_json::to_string(&state)?)?;
        Ok(state.window_anchor)
    })
    .map_err(|err| err.to_string())
}

pub fn save_anchor(eja: &Eja, anchor: Id) -> Result<(), String> {
    update_state(eja, |_, state| {
        state.window_anchor = anchor.max(0);
        Ok(())
    })
    .map_err(|err| err.to_string())
}

pub fn next_trigger(eja: &Eja) -> eja_core::Result<Option<(Id, Source)>> {
    let state = decode_state(eja.get_meta(META_STATE)?)?;
    let cursor = state.turn_cursor;
    let head = eja.revision()?;
    Ok(find_trigger(
        &eja.entries_after(cursor, head)?,
        &state.world_triggers,
    ))
}

/// Atomically record an optional reply and advance the host turn cursor for `trigger`.
pub fn complete_turn(eja: &Eja, trigger: Id, reply: &str) -> eja_core::Result<()> {
    update_state(eja, |tx, state| {
        let cursor = state.turn_cursor;
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
        state.turn_cursor = trigger;
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

pub fn register_check(
    eja: &Eja,
    name: &str,
    file: &str,
    equals: Option<&str>,
    contains: Option<&str>,
    not_equals: Option<&str>,
) -> eja_core::Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Invalid("本地检查仅支持 macOS".into()));
    }
    let has_predicate = equals.is_some() || contains.is_some() || not_equals.is_some();
    let valid_exact =
        |val: Option<&str>| val.is_none_or(|s| s.len() <= 4096 && !s.ends_with(['\r', '\n']));
    let valid_contains = |val: Option<&str>| val.is_none_or(|s| !s.is_empty() && s.len() <= 4096);
    if name.is_empty()
        || name.len() > 40
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
        || file.len() > 255
        || validate_single_name(file).is_err()
        || !has_predicate
        || !valid_exact(equals)
        || !valid_contains(contains)
        || !valid_exact(not_equals)
    {
        return Err(Error::Invalid("检查名限 1–40 个小写字母、数字、下划线或连字符；文件限数据目录中的单个文件名；至少提供 equals、contains（非空）或 not_equals 之一，文本上限 4096 字节且精确比较项不能以换行结尾".into()));
    }
    let check = FileCheck {
        file: file.into(),
        equals: equals.map(Into::into),
        contains: contains.map(Into::into),
        not_equals: not_equals.map(Into::into),
    };
    update_state(eja, |_, state| {
        if let Some(previous) = state.checks.get(name) {
            return if previous == &check {
                Ok(())
            } else {
                Err(Error::Invalid("检查名称已绑定，请使用新名称".into()))
            };
        }
        if state.checks.len() >= 64 {
            return Err(Error::Invalid("已达到 64 条检查规则上限".into()));
        }
        state.checks.insert(name.into(), check);
        Ok(())
    })
}

pub fn registered_check(eja: &Eja, name: &str) -> eja_core::Result<FileCheck> {
    decode_state(eja.get_meta(META_STATE)?)?
        .checks
        .remove(name)
        .ok_or_else(|| Error::Invalid(format!("未知检查规则 {name}")))
}

pub fn keep_with_check(
    eja: &Eja,
    concern: Option<Id>,
    text: &str,
    checker: Option<&str>,
) -> eja_core::Result<Entry> {
    if text.trim().is_empty() {
        return Err(Error::Invalid("挂念需要非空文本".into()));
    }
    update_state(eja, |tx, state| {
        if let Some(checker) = checker {
            if !state.checks.contains_key(checker) {
                return Err(Error::Invalid(format!("未知检查规则 {checker}")));
            }
        }
        let bound = concern
            .and_then(|id| state.concern_checks.get(&id))
            .map(String::as_str);
        let entry = match checker.or(bound) {
            Some(name) => {
                let body: Vec<&str> = text
                    .lines()
                    .filter(|line| !line.trim().starts_with("验收规则："))
                    .collect();
                let clean = body.join("\n");
                let clean = clean.trim();
                if clean.is_empty() {
                    return Err(Error::Invalid("挂念需要非空文本".into()));
                }
                tx.keep(concern, format!("{clean}\n验收规则：{name}"))?
            }
            None => tx.keep(concern, text)?,
        };
        if let Some(name) = checker {
            state
                .concern_checks
                .insert(concern.unwrap_or(entry.id), name.into());
        }
        Ok(entry)
    })
}

fn match_notice(checker: &str) -> String {
    format!("检查规则 {checker} 已命中")
}

pub fn release_with_witness(
    eja: &Eja,
    concern: Id,
    witness: Option<Id>,
) -> eja_core::Result<Entry> {
    update_state(eja, |tx, state| {
        if let Some(checker) = state.concern_checks.get(&concern) {
            let basis = tx.evidence(concern)?;
            let since = basis.latest.unwrap_or(basis.entry).id;
            let witness = witness
                .filter(|id| *id > since)
                .ok_or_else(|| Error::Invalid(format!("需要检查规则 {checker} 的新命中记录")))?;
            let evidence = tx.evidence(witness)?;
            if !matches!(evidence.entry.fact, Fact::Noticed { source: Source::World, text }
                    if text == match_notice(checker))
            {
                return Err(Error::Invalid(format!(
                    "证据须来自检查规则 {checker} 的命中记录"
                )));
            }
        }
        let entry = tx.release(concern)?;
        if let Some(checker) = state.concern_checks.remove(&concern) {
            let still_bound = state.concern_checks.values().any(|item| item == &checker);
            if !still_bound {
                state.wakes.retain(|wake| {
                    wake.guard
                        .as_ref()
                        .is_none_or(|guard| guard.checker != checker)
                });
            }
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
        let alarm_checker = alarm.guard.as_ref().map(|g| g.checker.as_str());
        existing.retain(|item| {
            item.id != alarm.id
                && item.reason != alarm.reason
                && (alarm_checker.is_none()
                    || item.guard.as_ref().map(|g| g.checker.as_str()) != alarm_checker)
        });
        existing.push(alarm.clone());
    }
    existing.sort_by(|a, b| a.fire_at_unix.cmp(&b.fire_at_unix).then(a.id.cmp(&b.id)));
    Ok(())
}

pub fn schedule_wakes(eja: &Eja, alarms: &[WakeAlarm]) -> eja_core::Result<()> {
    update_state(eja, |_, state| merge_wakes(&mut state.wakes, alarms))
}

pub fn cancel_wakes(
    eja: &Eja,
    reason: &str,
    guard_checker: Option<&str>,
) -> eja_core::Result<usize> {
    update_state(eja, |_, state| {
        let before = state.wakes.len();
        state.wakes.retain(|item| {
            item.reason != reason
                && (guard_checker.is_none()
                    || item.guard.as_ref().map(|g| g.checker.as_str()) != guard_checker)
        });
        Ok(before - state.wakes.len())
    })
}

pub fn pending_wakes(eja: &Eja) -> eja_core::Result<Vec<WakeAlarm>> {
    Ok(decode_state(eja.get_meta(META_STATE)?)?.wakes)
}

pub fn next_wake_at(eja: &Eja) -> eja_core::Result<Option<i64>> {
    Ok(pending_wakes(eja)?.first().map(|alarm| alarm.fire_at_unix))
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

pub fn settle_guarded_wake(
    eja: &Eja,
    alarm: &WakeAlarm,
    matched: bool,
    retry_at: Option<(i64, String)>,
) -> eja_core::Result<bool> {
    update_state(eja, |tx, state| {
        let Some(index) = state.wakes.iter().position(|item| item == alarm) else {
            return Ok(false);
        };
        let mut alarm = state.wakes.remove(index);
        let Some(guard) = alarm.guard.as_mut() else {
            return Err(Error::Conflict);
        };
        if matched {
            let id = tx.notice(Source::World, match_notice(&guard.checker))?;
            state.world_triggers.push(id);
        } else if let Some((at, display)) = retry_at {
            guard.remaining -= 1;
            alarm.fire_at_unix = at;
            alarm.fire_at_display = display;
            state.wakes.push(alarm);
            state.wakes.sort_by(|left, right| {
                left.fire_at_unix
                    .cmp(&right.fire_at_unix)
                    .then(left.id.cmp(&right.id))
            });
        }
        Ok(matched)
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
    fn collect_due_wakes_uses_subject_clock_and_removes_fired_alarms() {
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
        let (fired, guarded, _) = collect_due_wakes(&eja).unwrap();
        assert!(fired && guarded.is_empty());
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
        let (fired_again, _, _) = collect_due_wakes(&eja).unwrap();
        assert!(!fired_again);
    }
}
