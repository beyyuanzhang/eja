//! Capabilities behind the single core port. The core only knows `Lookup`.
use crate::{tasks, Event, LookSource, Lookup, Result, Selector};
use serde_json::Value;

pub trait ArtifactReader {
    fn slice(&self, path: &str, selector: &Selector, control: &crate::Control) -> Result<Value>;
}

/// Host-declared read-only capabilities, under the caller's control.
pub trait Observer: Send + Sync {
    fn observe(&self, tool: &str, arguments: &Value, control: &crate::Control) -> Result<Value>;
}

/// Host composition: one lookup over the three sources an EJA deployment has.
#[derive(Default, Clone, Copy)]
pub struct Ports<'a> {
    pub node: Option<&'a dyn ArtifactReader>,
    pub world: Option<&'a dyn Observer>,
}

impl Lookup for Ports<'_> {
    fn look(
        &self,
        source: LookSource,
        target: &str,
        arguments: &Value,
        selector: &Selector,
        log: &[Event],
        control: &crate::Control,
    ) -> Result<Value> {
        control.check()?;
        match source {
            LookSource::Log => {
                let events: Vec<&Event> = match target {
                    "history" | "all" => log
                        .iter()
                        .filter(|event| {
                            target == "all"
                                || matches!(event.kind, crate::Kind::User | crate::Kind::Eja)
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
                    let mut page = super::text_page(
                        &serde_json::to_string(events[0])?,
                        *from,
                        *bytes,
                        crate::aja::FRAME_LIMIT / 8,
                    );
                    page["event_id"] = serde_json::json!(events[0].id);
                    return Ok(page);
                }
                let (from, count) = match selector {
                    Selector::Whole => (events.len().saturating_sub(40), 40),
                    Selector::Range { from, lines } => {
                        ((*from).min(events.len()), (*lines).min(1000))
                    }
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
                        let locator = serde_json::json!({"id":event.id,"bytes":size - 1});
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
            LookSource::Node => {
                let node = self.node.ok_or("no node reader is configured")?;
                node.slice(target, selector, control)
            }
            LookSource::World => {
                let world = self.world.ok_or("no world reader is configured")?;
                if !matches!(selector, Selector::Whole) {
                    return Err("world tools select their result through arguments".into());
                }
                world.observe(target, arguments, control)
            }
        }
    }
}
