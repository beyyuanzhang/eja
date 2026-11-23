//! Host-owned scheduling, turn settlement, window anchor, and upgrade state stored
//! atomically alongside `eja-core` facts via `host_meta`.
use crate::sandbox::{validate_single_name, TrustedDir};
use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Source, Tx, FACT_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

const META_STATE: &str = "host.state";
const MAX_PROACTIVE_HISTORY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FileCheck {
    file: String,
    equals: String,
}

impl FileCheck {
    pub fn matches(&self, data_dir: &TrustedDir) -> std::io::Result<bool> {
        match data_dir.read_check_file(&self.file, 4096) {
            Ok(bytes) => Ok(std::str::from_utf8(&bytes)
                .map_err(std::io::Error::other)?
                .trim_end_matches(['\r', '\n'])
                == self.equals),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum ConcernCheck {
    Registered(String),
    Legacy {
        #[serde(default)]
        require_witness: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeGuard {
    #[serde(default)]
    pub checker: String,
    pub interval_seconds: i64,
    #[serde(default)]
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
    concern_checks: BTreeMap<Id, ConcernCheck>,
    #[serde(default)]
    checks: BTreeMap<String, FileCheck>,
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
    entries
        .retain(|entry| entry.id <= trigger || is_trigger_entry(entry, &world_triggers).is_none());
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

        let previous_wakes = state.wakes.len();
        state.wakes.retain(|alarm| {
            alarm
                .guard
                .as_ref()
                .is_none_or(|guard| !guard.checker.is_empty())
        });
        if previous_wakes != state.wakes.len() {
            tx.notice(Source::World, "旧守望已停用，请重新绑定本地检查规则。")?;
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

pub fn register_check(eja: &Eja, name: &str, file: &str, equals: &str) -> eja_core::Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Invalid("本地检查仅支持 macOS".into()));
    }
    if name.is_empty()
        || name.len() > 40
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
        || file.len() > 255
        || validate_single_name(file).is_err()
        || equals.len() > 4096
        || equals.ends_with(['\r', '\n'])
    {
        return Err(Error::Invalid("检查名限 1–40 个小写字母、数字、下划线或连字符；文件限数据目录中的单个文件名；期望文本上限 4096 字节且不能以换行结尾".into()));
    }
    let check = FileCheck {
        file: file.into(),
        equals: equals.into(),
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
        let bound = concern.and_then(|id| match state.concern_checks.get(&id) {
            Some(ConcernCheck::Registered(name)) => Some(name.as_str()),
            _ => None,
        });
        let entry = match checker.or(bound) {
            Some(name) => tx.keep(concern, format!("{text}\n验收规则：{name}"))?,
            None => tx.keep(concern, text)?,
        };
        if let Some(name) = checker {
            state.concern_checks.insert(
                concern.unwrap_or(entry.id),
                ConcernCheck::Registered(name.into()),
            );
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
        match state.concern_checks.get(&concern) {
            Some(ConcernCheck::Registered(checker)) => {
                let basis = tx.evidence(concern)?;
                let since = basis.latest.unwrap_or(basis.entry).id;
                let witness = witness.filter(|id| *id > since).ok_or_else(|| {
                    Error::Invalid(format!("需要检查规则 {checker} 的新命中记录"))
                })?;
                let evidence = tx.evidence(witness)?;
                if !matches!(evidence.entry.fact, Fact::Noticed { source: Source::World, text }
                    if text == match_notice(checker))
                {
                    return Err(Error::Invalid(format!(
                        "证据须来自检查规则 {checker} 的命中记录"
                    )));
                }
            }
            Some(ConcernCheck::Legacy {
                require_witness: true,
            }) => {
                return Err(Error::Invalid(
                    "旧验收条件需要通过 keep 重新绑定检查规则".into(),
                ));
            }
            _ => {}
        }
        let entry = tx.release(concern)?;
        state.concern_checks.remove(&concern);
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
    fn checkpoint_upgrade_removes_script_watches_and_keeps_existing_obligations() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = TrustedDir::open(dir.path()).unwrap();
        let eja = Eja::open(":memory:").unwrap();
        let concern = eja.keep(None, "existing concern").unwrap();
        let plain = serde_json::json!({
            "id": "plain", "fire_at_unix": 100, "fire_at_display": "00:01:40", "reason": "reminder"
        });
        let mut old = plain.clone();
        old["id"] = serde_json::json!("script");
        old["guard"] = serde_json::json!({
            "probe": {"kind":"command", "command":"true"}, "interval_seconds":10
        });
        let state = serde_json::json!({
            "turn_cursor": concern.id,
            "wakes": [plain, old],
            "concern_checks": {(concern.id.to_string()): {"require_witness": true}}
        });
        eja.set_meta(META_STATE, &state.to_string()).unwrap();
        bootstrap(&eja, &core_dir, None, "1", 1).unwrap();
        let wakes = pending_wakes(&eja).unwrap();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].id, "plain");
        assert_eq!(next_trigger(&eja).unwrap(), None);
        assert!(release_with_witness(&eja, concern.id, None).is_err());
    }

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
