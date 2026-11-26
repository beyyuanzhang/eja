//! The journal contract between file observations and their consumers.
use eja_core::{Id, Result, Source, Tx};

fn hit_key(id: Id) -> String {
    format!("host.check-hit:{id}")
}

pub(super) fn publish(tx: &Tx<'_>, rule: &str) -> Result<Id> {
    let id = tx.notice(Source::World, format!("检查规则 {rule} 已命中"))?;
    tx.set_meta(&hit_key(id), rule)?;
    Ok(id)
}

pub(super) fn read_hit(
    id: Id,
    get_meta: impl FnOnce(&str) -> Result<Option<String>>,
) -> Result<Option<String>> {
    get_meta(&hit_key(id))
}
