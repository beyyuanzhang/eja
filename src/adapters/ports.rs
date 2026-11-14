//! Host surface: one read over the three authorities, and the capabilities a
//! host may add on top of it.
use super::Worlds;
use crate::{tasks, Address, Authority, Control, Event, Host, Result, Selector, TaskId, Witness};
use serde_json::Value;

pub trait RecordReader {
    fn slice(&self, path: &str, selector: &Selector, control: &Control) -> Result<Value>;
    /// One region of one record: `aja/entry` names a tool result, `aja/path` a
    /// frozen file. Reading either is one address lookup.
    fn entry(
        &self,
        _aja: u64,
        _entry: usize,
        _selector: &Selector,
        _control: &Control,
    ) -> Result<Value> {
        Err("this reader does not address record entries".into())
    }
}

/// Host-declared read-only capabilities, under the caller's control.
pub trait Observer: Send + Sync {
    fn observe(&self, tool: &str, arguments: &Value, control: &Control) -> Result<Value>;
}

/// Host composition: what one deployment can read and observe.
#[derive(Default, Clone, Copy)]
pub struct Ports<'a> {
    pub aja: Option<&'a dyn RecordReader>,
    pub world: Option<&'a dyn Observer>,
    pub witness: Option<&'a dyn Witness>,
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
                .world
                .ok_or("no world reader is configured")?
                .observe(target, arguments, control);
        };
        match address.authority {
            Authority::Log => page(log, &address.id, selector),
            Authority::Record => {
                let (aja, rest) = address
                    .id
                    .split_once('/')
                    .ok_or("record addresses are aja/entry or aja/path")?;
                let aja: u64 = aja
                    .parse()
                    .map_err(|_| "record address needs a numeric aja")?;
                let reader = self.aja.ok_or("no record reader is configured")?;
                match rest.parse::<usize>() {
                    Ok(entry) => reader.entry(aja, entry, selector, control),
                    Err(_) => reader.slice(&address.id, selector, control),
                }
            }
            Authority::World => {
                if !matches!(selector, Selector::Whole) {
                    return Err("a world address names one path; select through arguments".into());
                }
                self.world.ok_or("no world reader is configured")?.observe(
                    "read_file",
                    &serde_json::json!({"path": address.id}),
                    control,
                )
            }
        }
    }

    fn witness(&self, request: &Value, control: &Control) -> Result<Value> {
        self.witness
            .ok_or("this host observes no acceptance evidence")?
            .observe(request, control)
    }

    fn reconcile(&self, task: TaskId, control: &Control) -> Result<Value> {
        self.worlds
            .ok_or("this host isolates no worlds")?
            .reconcile(task, control)
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
                if !tasks(log).iter().any(|task| task.id == id) {
                    return Err("no such task".into());
                }
                let scope = format!("task:{id}");
                log.iter().filter(|event| event.scope == scope).collect()
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
    let mut bytes = 512;
    let mut next = from;
    for event in &events[from..end] {
        let size = serde_json::to_vec(event)?.len() + 1;
        if size > crate::aja::FRAME_LIMIT - 512 {
            let locator = serde_json::json!({
                "id": event.id,
                "bytes": size - 1,
                "address": format!("log:event:{}", event.id),
            });
            let size = locator.to_string().len() + 1;
            if size > crate::aja::FRAME_LIMIT.saturating_sub(bytes) {
                break;
            }
            bytes += size;
            partial.push(locator);
        } else {
            if size > crate::aja::FRAME_LIMIT.saturating_sub(bytes) {
                break;
            }
            bytes += size;
            selected.push(*event);
        }
        next += 1;
    }
    Ok(
        serde_json::json!({"events":selected,"partial_events":partial,"from":from,"next":next,"total":events.len(),"truncated":next<events.len()}),
    )
}
