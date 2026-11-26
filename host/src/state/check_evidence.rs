//! The journal contract between file observations and their consumers.
use super::write_meta;
use eja_core::{Id, Result, Source, Tx};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CheckId {
    pub name: String,
    pub revision: String,
}

fn hit_key(id: Id) -> String {
    format!("host.check-hit:{id}")
}

fn notice_text(checker: &str) -> String {
    format!("检查规则 {checker} 已命中")
}

pub(super) fn publish(tx: &Tx<'_>, check: &CheckId) -> Result<()> {
    let id = tx.notice(Source::World, notice_text(&check.name))?;
    write_meta(tx, &hit_key(id), check)
}

pub(super) fn read_hit(
    id: Id,
    get_meta: impl FnOnce(&str) -> Result<Option<String>>,
) -> Result<Option<CheckId>> {
    get_meta(&hit_key(id))?
        .map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
