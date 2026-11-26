//! Owns concern bindings and consumes journal evidence.
use super::{check_evidence, decode_meta, write_meta};
use eja_core::{Eja, Entry, Error, Id, Result};
use std::collections::BTreeMap;

const META_BINDINGS: &str = "host.concern-bindings";
type Bindings = BTreeMap<Id, String>;

pub fn keep_with_check(
    eja: &Eja,
    concern: Option<Id>,
    text: &str,
    checker: Option<&str>,
) -> Result<Entry> {
    if text.trim().is_empty() {
        return Err(Error::Invalid("挂念需要非空文本".into()));
    }
    eja.transaction(|tx| {
        let mut bindings: Bindings = decode_meta(tx.get_meta(META_BINDINGS)?)?;
        let bound = concern.and_then(|id| bindings.get(&id)).map(String::as_str);
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
        if let Some(check) = checker {
            bindings.insert(concern.unwrap_or(entry.id), check.to_owned());
            write_meta(tx, META_BINDINGS, &bindings)?;
        }
        Ok(entry)
    })
}

pub fn release_with_witness(eja: &Eja, concern: Id, witness: Option<Id>) -> Result<Entry> {
    eja.transaction(|tx| {
        let mut bindings: Bindings = decode_meta(tx.get_meta(META_BINDINGS)?)?;
        let Some(rule) = bindings.remove(&concern) else {
            return tx.release(concern);
        };

        let basis = tx.evidence(concern)?;
        let since = basis.latest.unwrap_or(basis.entry).id;
        let witness = witness
            .filter(|id| *id > since)
            .ok_or_else(|| Error::Invalid(format!("需要检查规则 {rule} 的新命中记录")))?;
        if check_evidence::read_hit(witness, |key| tx.get_meta(key))?.as_deref()
            != Some(rule.as_str())
        {
            return Err(Error::Invalid(format!(
                "证据须来自检查规则 {rule} 的命中记录"
            )));
        }

        let entry = tx.release_with_witness(concern, Some(witness))?;
        write_meta(tx, META_BINDINGS, &bindings)?;
        Ok(entry)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eja_core::Fact;

    #[test]
    fn release_requires_a_fresh_hit_from_the_bound_rule() {
        let eja = Eja::open(":memory:").unwrap();
        let record = |rule| {
            eja.transaction(|tx| check_evidence::publish(tx, rule))
                .unwrap()
        };
        let concern = keep_with_check(&eja, None, "waiting", Some("ready"))
            .unwrap()
            .id;
        let stale = record("ready");
        keep_with_check(&eja, Some(concern), "revised", None).unwrap();
        let other = record("another_rule");
        assert!(release_with_witness(&eja, concern, Some(stale)).is_err());
        assert!(release_with_witness(&eja, concern, Some(other)).is_err());

        let witness = record("ready");
        assert_eq!(
            release_with_witness(&eja, concern, Some(witness))
                .unwrap()
                .fact,
            Fact::Released {
                concern,
                witness_fact_id: Some(witness)
            }
        );
    }
}
