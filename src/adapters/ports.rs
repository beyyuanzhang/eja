//! Host surface: one read over the three authorities, and the capabilities a
//! host may add on top of it.
use super::Worlds;
use crate::{Address, Authority, Control, Event, Host, Result, Selector, TaskId};
use serde_json::Value;

pub trait RecordReader {
    fn slice(&self, path: &str, selector: &Selector, control: &Control) -> Result<Value>;
}

/// What the host declares EJA may do.
///
/// An observation is always safe; a witness is an effect-capable host act and
/// must be declared, so both refuse by default.
pub trait Capabilities: Send + Sync {
    fn observe(&self, _tool: &str, _arguments: &Value, _control: &Control) -> Result<Value> {
        Err("this host observes no tools".into())
    }

    fn witness(&self, _request: &Value, _control: &Control) -> Result<Value> {
        Err("this host observes no acceptance evidence".into())
    }
}

/// Host composition: what one deployment can read and observe.
#[derive(Default, Clone, Copy)]
pub struct Ports<'a> {
    pub aja: Option<&'a dyn RecordReader>,
    pub capabilities: Option<&'a dyn Capabilities>,
    pub worlds: Option<&'a dyn Worlds>,
}

impl Host for Ports<'_> {
    fn look(
        &self,
        target: &str,
        arguments: &Value,
        selector: &Selector,
        log: &[Event],
        control: &Control,
    ) -> Result<Value> {
        control.check()?;
        let Some(address) = Address::parse(target) else {
            // Not an address: a host read tool, selected through its arguments.
            if !matches!(selector, Selector::Whole) {
                return Err("host tools select their result through arguments".into());
            }
            return self
                .capabilities
                .ok_or("no host capabilities are configured")?
                .observe(target, arguments, control);
        };
        match address.authority {
            Authority::Log => page(log, &address.id, selector),
            Authority::Record => self.aja.ok_or("no record reader is configured")?.slice(
                &address.id,
                selector,
                control,
            ),
            Authority::World => {
                if !matches!(selector, Selector::Whole) {
                    return Err("a world address names one path; select through arguments".into());
                }
                self.capabilities
                    .ok_or("no host capabilities are configured")?
                    .observe(
                        "read_file",
                        &serde_json::json!({"path": address.id}),
                        control,
                    )
            }
        }
    }

    fn witness(&self, request: &Value, control: &Control) -> Result<Value> {
        self.capabilities
            .ok_or("this host observes no acceptance evidence")?
            .witness(request, control)
    }

    fn reconcile(
        &self,
        task: TaskId,
        root: Option<&std::path::Path>,
        control: &Control,
    ) -> Result<Value> {
        self.worlds
            .ok_or("this host isolates no worlds")?
            .reconcile(task, root, control)
    }
}

/// A log page: whole pages the newest events, range pages from an index, and a
/// byte view of one oversized event. Every page names where to continue.
fn page(log: &[Event], target: &str, selector: &Selector) -> Result<Value> {
    let events: Vec<&Event> = match target {
        "history" | "all" => log
            .iter()
            .filter(|event| {
                target == "all" || matches!(event.kind, crate::Kind::User | crate::Kind::Eja)
            })
            .collect(),
        _ => {
            if let Some(raw) = target.strip_prefix("task:") {
                let id: u64 = raw.parse().map_err(|_| "task target needs an id")?;
                let scope = format!("task:{id}");
                let scoped: Vec<&Event> = log.iter().filter(|event| event.scope == scope).collect();
                if scoped.is_empty() {
                    return Err("no such task".into());
                }
                scoped
            } else if let Some(raw) = target.strip_prefix("event:") {
                let id: u64 = raw.parse().map_err(|_| "event target needs an id")?;
                vec![log
                    .iter()
                    .find(|event| event.id == id)
                    .ok_or("no such event")?]
            } else {
                return Err("log targets history, all, task:<id> or event:<id>".into());
            }
        }
    };
    if let Selector::Bytes { from, bytes } = selector {
        if !target.starts_with("event:") {
            return Err("byte views target event:<id>".into());
        }
        let mut view = super::text_page(
            &serde_json::to_string(events[0])?,
            *from,
            *bytes,
            crate::aja::FRAME_LIMIT / 8,
        );
        view["event_id"] = serde_json::json!(events[0].id);
        return Ok(view);
    }
    let (from, count) = match selector {
        Selector::Whole => (events.len().saturating_sub(40), 40),
        Selector::Range { from, lines } => ((*from).min(events.len()), (*lines).min(1000)),
        _ => return Err("log pages use whole or range".into()),
    };
    let end = from.saturating_add(count).min(events.len());
    let mut selected = Vec::new();
    let mut partial = Vec::new();
    // One budget helper for every address space: a log page, a record window and
    // a single passage all admit units and stop the same way.
    let mut budget = crate::measure::Budget::new(crate::aja::FRAME_LIMIT);
    budget.admit(512);
    let mut next = from;
    for event in &events[from..end] {
        let size = crate::measure::bytes(event)? + 1;
        // An event that cannot fit a frame on its own is stated by its address;
        // the reader continues with a byte selector instead of losing it.
        if size > crate::aja::FRAME_LIMIT - 512 {
            let locator = serde_json::json!({
                "id": event.id,
                "bytes": size - 1,
                "address": format!("log:event:{}", event.id),
            });
            if !budget.admit(crate::measure::value(&locator) + 1) {
                break;
            }
            partial.push(locator);
        } else {
            if !budget.admit(size) {
                break;
            }
            selected.push(*event);
        }
        next += 1;
    }
    let truncated = budget.truncated() || next < events.len();
    Ok(
        serde_json::json!({"events":selected,"partial_events":partial,"from":from,"next":next,"total":events.len(),"truncated":truncated}),
    )
}
