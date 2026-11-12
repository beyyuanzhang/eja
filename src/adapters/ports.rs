//! Capabilities behind the single core port. The core only knows `Lookup`.
use crate::{tasks, ArtifactContent, Event, LookSource, Lookup, Result, Selector};
use serde_json::Value;

pub trait Retriever {
    fn retrieve(&self, query: &str, events: &[Event]) -> Result<Vec<u64>>;
}

pub struct NoopRetriever;

impl Retriever for NoopRetriever {
    fn retrieve(&self, _query: &str, _events: &[Event]) -> Result<Vec<u64>> {
        Ok(Vec::new())
    }
}

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
    pub log: Option<&'a dyn Retriever>,
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
                let retriever = self.log.ok_or("no log reader is configured")?;
                match selector {
                    Selector::Search { query } => {
                        let ids = retriever.retrieve(query, log)?;
                        let events: Vec<&Event> =
                            log.iter().filter(|event| ids.contains(&event.id)).collect();
                        Ok(serde_json::json!({"matches": events.len(), "events": events}))
                    }
                    Selector::Whole => {
                        if let Some(raw) = target.strip_prefix("task:") {
                            let id: u64 = raw.parse().map_err(|_| "task target needs an id")?;
                            let task = tasks(log)
                                .into_iter()
                                .find(|task| task.id == id)
                                .ok_or("no such task")?;
                            return Ok(serde_json::to_value(task)?);
                        }
                        if let Some(raw) = target.strip_prefix("event:") {
                            let id: u64 = raw.parse().map_err(|_| "event target needs an id")?;
                            let event = log
                                .iter()
                                .find(|event| event.id == id)
                                .ok_or("no such event")?;
                            return Ok(serde_json::to_value(event)?);
                        }
                        Err("a log look targets task:<id>, event:<id>, or a search".into())
                    }
                    _ => Err("a log look uses whole or search".into()),
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
