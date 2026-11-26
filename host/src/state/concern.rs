//! Owns concern bindings and consumes journal evidence.
use super::{
    check_evidence::{self, CheckId},
    decode_meta, write_meta,
};
use eja_core::{Eja, Entry, Error, Id, Result};
use std::collections::BTreeMap;

const META_BINDINGS: &str = "host.concern-bindings";
type Bindings = BTreeMap<Id, CheckId>;

pub fn keep_with_check(
    eja: &Eja,
    concern: Option<Id>,
    text: &str,
    checker: Option<CheckId>,
) -> Result<Entry> {
    if text.trim().is_empty() {
        return Err(Error::Invalid("挂念需要非空文本".into()));
    }
    eja.transaction(|tx| {
        let mut bindings: Bindings = decode_meta(tx.get_meta(META_BINDINGS)?)?;
        let bound = concern.and_then(|id| bindings.get(&id));
        let rule = checker.as_ref().or(bound);
        let entry = match rule.map(|check| check.name.as_str()) {
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
            bindings.insert(concern.unwrap_or(entry.id), check);
            write_meta(tx, META_BINDINGS, &bindings)?;
        }
        Ok(entry)
    })
}

pub fn release_with_witness(eja: &Eja, concern: Id, witness: Option<Id>) -> Result<Entry> {
    eja.transaction(|tx| {
        let mut bindings: Bindings = decode_meta(tx.get_meta(META_BINDINGS)?)?;
        let Some(binding) = bindings.remove(&concern) else {
            return tx.release(concern);
        };

        let name = &binding.name;
        let basis = tx.evidence(concern)?;
        let since = basis.latest.unwrap_or(basis.entry).id;
        let witness = witness
            .filter(|id| *id > since)
            .ok_or_else(|| Error::Invalid(format!("需要检查规则 {name} 的新命中记录")))?;
        if check_evidence::read_hit(witness, |key| tx.get_meta(key))?.as_ref() != Some(&binding) {
            return Err(Error::Invalid(format!(
                "证据须来自检查规则 {name} 的命中记录"
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
    fn release_accepts_only_a_fresh_hit_for_the_bound_rule_revision() {
        let eja = Eja::open(":memory:").unwrap();
        let check = |revision: &str| CheckId {
            name: "ready".into(),
            revision: revision.into(),
        };
        let record = |rule: &CheckId| {
            eja.transaction(|tx| {
                check_evidence::publish(tx, rule)?;
                tx.revision()
            })
            .unwrap()
        };

        let concern = keep_with_check(&eja, None, "waiting", Some(check("one")))
            .unwrap()
            .id;
        let stale = record(&check("one"));
        keep_with_check(&eja, Some(concern), "revised", None).unwrap();
        let wrong_revision = record(&check("two"));
        assert!(release_with_witness(&eja, concern, Some(stale)).is_err());
        assert!(release_with_witness(&eja, concern, Some(wrong_revision)).is_err());

        let witness = record(&check("one"));
        assert_eq!(
            release_with_witness(&eja, concern, Some(witness))
                .unwrap()
                .fact,
            Fact::Released {
                concern,
                witness_fact_id: Some(witness),
            }
        );
    }
}
