//! Owns registered file rules and the pending wake queue.
use super::{check_evidence, decode_meta, write_meta};
use crate::sandbox::{validate_single_name, TrustedDir};
use eja_core::{Eja, Error, Fact, Result, Source, Tx, FACT_BYTES};
use serde::{Deserialize, Serialize};

const META_WAKES: &str = "host.wakes";
const MAX_GUARDED_WAKES: usize = 64;

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

fn rule_key(name: &str) -> String {
    format!("host.check-rule:{name}")
}

fn update_wakes<T>(
    eja: &Eja,
    f: impl FnOnce(&Tx<'_>, &mut Vec<WakeAlarm>) -> Result<T>,
) -> Result<T> {
    eja.transaction(|tx| {
        let mut wakes = decode_meta(tx.get_meta(META_WAKES)?)?;
        let result = f(tx, &mut wakes)?;
        write_meta(tx, META_WAKES, &wakes)?;
        Ok(result)
    })
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
    eja.transaction(|tx| {
        let key = rule_key(name);
        if let Some(raw) = tx.get_meta(&key)? {
            return if serde_json::from_str::<FileCheck>(&raw)? == check {
                Ok(())
            } else {
                Err(Error::Invalid("检查名称已绑定，请使用新名称".into()))
            };
        }
        write_meta(tx, &key, &check)
    })
}

pub fn registered_check(eja: &Eja, name: &str) -> Result<FileCheck> {
    let raw = eja
        .get_meta(&rule_key(name))?
        .ok_or_else(|| Error::Invalid(format!("未知检查规则 {name}")))?;
    Ok(serde_json::from_str(&raw)?)
}

fn wake_notice_text(reason: &str) -> String {
    format!("定时唤醒：{reason}")
}

fn merge_wakes(existing: &mut Vec<WakeAlarm>, alarms: &[WakeAlarm]) -> Result<()> {
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
    if existing
        .iter()
        .filter(|alarm| alarm.guard.is_some())
        .count()
        > MAX_GUARDED_WAKES
    {
        return Err(Error::Invalid("已达到 64 条待执行守望上限".into()));
    }
    existing.sort_by(|a, b| a.fire_at_unix.cmp(&b.fire_at_unix).then(a.id.cmp(&b.id)));
    Ok(())
}

pub fn schedule_wakes(eja: &Eja, alarms: &[WakeAlarm]) -> Result<()> {
    update_wakes(eja, |_, wakes| merge_wakes(wakes, alarms))
}

pub fn cancel_wakes(eja: &Eja, reason: &str, guard_checker: Option<&str>) -> Result<usize> {
    update_wakes(eja, |_, wakes| {
        let before = wakes.len();
        wakes.retain(|item| {
            item.reason != reason
                && (guard_checker.is_none()
                    || item.guard.as_ref().map(|g| g.checker.as_str()) != guard_checker)
        });
        Ok(before - wakes.len())
    })
}

pub fn pending_wakes(eja: &Eja) -> Result<Vec<WakeAlarm>> {
    decode_meta(eja.get_meta(META_WAKES)?)
}

pub fn dismiss_wake(eja: &Eja, id: &str) -> Result<bool> {
    update_wakes(eja, |tx, wakes| {
        let Some(index) = wakes.iter().position(|alarm| alarm.id == id) else {
            return Ok(false);
        };
        let alarm = wakes.remove(index);
        let reason = &alarm.reason[..alarm.reason.floor_char_boundary(1024)];
        tx.notice(
            Source::World,
            format!("用户取消了{}", wake_notice_text(reason)),
        )?;
        Ok(true)
    })
}

pub fn next_wake_at(eja: &Eja) -> Result<Option<i64>> {
    Ok(pending_wakes(eja)?.first().map(|alarm| alarm.fire_at_unix))
}

pub fn tick(
    eja: &Eja,
    data_dir: &TrustedDir,
    format_at: impl Fn(i64) -> String,
    mut report_error: impl FnMut(String),
) -> Result<(bool, bool)> {
    let (mut fired, due_guarded, now) = collect_due_wakes(eja)?;
    let checked_guards = !due_guarded.is_empty();
    for alarm in due_guarded {
        let guard = alarm.guard.as_ref().ok_or(Error::Conflict)?;
        let observation = registered_check(eja, &guard.checker)
            .map_err(|err| err.to_string())
            .and_then(|check| check.matches(data_dir).map_err(|err| err.to_string()));
        let retry = match &observation {
            Ok(false) if guard.remaining > 1 => now
                .checked_add(guard.interval_seconds)
                .map(|at| (at, format_at(at))),
            _ => None,
        };
        fired |= settle_guarded_wake(eja, &alarm, matches!(observation, Ok(true)), retry)?;
        if let Err(err) = observation {
            report_error(format!("守望「{}」已停止：{err}", alarm.reason));
        }
    }
    Ok((fired, checked_guards))
}

/// Atomically record due unguarded wakes and return due guarded wakes for out-of-tx probe evaluation.
fn collect_due_wakes(eja: &Eja) -> Result<(bool, Vec<WakeAlarm>, i64)> {
    eja.transaction(|tx| {
        let wakes: Vec<WakeAlarm> = decode_meta(tx.get_meta(META_WAKES)?)?;
        let now = tx.now()?.as_unix();
        let mut fired_plain = false;
        let mut due_guarded = Vec::new();
        let mut remaining = Vec::with_capacity(wakes.len());
        for alarm in wakes {
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
        if fired_plain {
            write_meta(tx, META_WAKES, &remaining)?;
        }
        Ok((fired_plain, due_guarded, now))
    })
}

fn settle_guarded_wake(
    eja: &Eja,
    alarm: &WakeAlarm,
    matched: bool,
    retry_at: Option<(i64, String)>,
) -> Result<bool> {
    update_wakes(eja, |tx, wakes| {
        let Some(index) = wakes.iter().position(|item| item == alarm) else {
            return Ok(false);
        };
        let mut alarm = wakes.remove(index);
        let Some(guard) = alarm.guard.as_mut() else {
            return Err(Error::Conflict);
        };
        if matched {
            check_evidence::publish(tx, &guard.checker)?;
        } else if let Some((at, display)) = retry_at {
            guard.remaining -= 1;
            alarm.fire_at_unix = at;
            alarm.fire_at_display = display;
            wakes.push(alarm);
            wakes.sort_by(|left, right| {
                left.fire_at_unix
                    .cmp(&right.fire_at_unix)
                    .then(left.id.cmp(&right.id))
            });
        }
        Ok(matched)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eja_core::Timestamp;

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

    #[test]
    fn dismissing_a_wake_with_a_maximal_reason_still_fits_the_notice() {
        let eja = Eja::open(":memory:").unwrap();
        let alarm = WakeAlarm {
            id: "w-big".into(),
            fire_at_unix: 100,
            fire_at_display: "later".into(),
            reason: "长".repeat((FACT_BYTES - 64) / 3),
            guard: None,
        };
        schedule_wakes(&eja, &[alarm]).unwrap();
        assert!(dismiss_wake(&eja, "w-big").unwrap());
        assert!(pending_wakes(&eja).unwrap().is_empty());
        assert!(!dismiss_wake(&eja, "w-big").unwrap());
    }
}
