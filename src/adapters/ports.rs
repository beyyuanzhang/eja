//! Capabilities behind the single core port. The core only knows `Lookup`.
use crate::{tasks, ArtifactContent, Event, LookSource, Lookup, Result, Selector};
use serde_json::Value;

pub trait ArtifactReader {
    fn read(&self, path: &str) -> ArtifactContent;

    /// A selected view of a node file. Readers that cannot slice still serve whole reads.
    fn slice(&self, path: &str, selector: &Selector) -> Result<Value> {
        if !matches!(selector, Selector::Whole) {
            return Err("this reader cannot select a range".into());
        }
        Ok(serde_json::to_value(self.read(path))?)
    }
}

pub struct NoopArtifactReader;

impl ArtifactReader for NoopArtifactReader {
    fn read(&self, path: &str) -> ArtifactContent {
        ArtifactContent::failed(path, "no artifact reader configured")
    }
}

/// Read-only observation of an external system. EJA may look; it never acts.
pub trait Observer: Send + Sync {
    fn observe(&self, tool: &str, arguments: &Value) -> Result<Value>;
}

pub struct NoopObserver;

impl Observer for NoopObserver {
    fn observe(&self, _: &str, _: &Value) -> Result<Value> {
        Err("no observer is configured".into())
    }
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
    ) -> Result<Value> {
        match source {
            LookSource::Log => {
                match selector {
                    Selector::Whole => {
                        // A log look always answers with events, so the shape does
                        // not depend on how the target was named.
                        if let Some(raw) = target.strip_prefix("task:") {
                            let id: u64 = raw.parse().map_err(|_| "task target needs an id")?;
                            if !tasks(log).iter().any(|task| task.id == id) {
                                return Err("no such task".into());
                            }
                            let scope = format!("task:{id}");
                            let events: Vec<&Event> =
                                log.iter().filter(|event| event.scope == scope).collect();
                            return Ok(serde_json::json!({"events": events}));
                        }
                        if let Some(raw) = target.strip_prefix("event:") {
                            let id: u64 = raw.parse().map_err(|_| "event target needs an id")?;
                            let event = log
                                .iter()
                                .find(|event| event.id == id)
                                .ok_or("no such event")?;
                            return Ok(serde_json::json!({"events": [event]}));
                        }
                        Err("a log look targets task:<id> or event:<id>".into())
                    }
                    _ => Err("a log look uses whole".into()),
                }
            }
            LookSource::Node => {
                let node = self.node.ok_or("no node reader is configured")?;
                node.slice(target, selector)
            }
            LookSource::World => {
                let world = self.world.ok_or("no world reader is configured")?;
                world.observe(target, arguments)
            }
        }
    }
}
