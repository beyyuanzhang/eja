use super::HostRuntime;
use crate::{config::ProviderKind, llm::cap_tool_result};
use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Result, Source};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{cmp::Reverse, collections::BTreeSet};

const CHANGE: &str = "eja.recall";

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct SearchTurn {
    pub batch: usize,
    visits: Vec<Visit>,
}

struct Visit {
    requested: String,
    effective: String,
    version: Option<Id>,
    through: Id,
    budget: usize,
    batch: usize,
    seen: Vec<Id>,
    receipts: Vec<Id>,
}

#[derive(Default, Deserialize, Serialize)]
struct Policy {
    active: Option<Id>,
    pending: Option<Candidate>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Cue {
    fact: Id,
    start: usize,
    end: usize,
}

#[derive(Deserialize, Serialize)]
struct Candidate {
    cue: Cue,
    case: Case,
}

#[derive(Deserialize, Serialize)]
struct Case {
    trigger: Id,
    through: Id,
    budget: usize,
    provider: ProviderKind,
    witness: Id,
    action: Id,
    retained: Vec<Id>,
}

fn key(query: &str) -> String {
    format!("recall:{:x}", Sha256::digest(query.to_ascii_lowercase()))
}

fn evidence_text(entry: &Entry) -> Option<&str> {
    match &entry.fact {
        Fact::Noticed {
            source: Source::Person | Source::World,
            text,
        }
        | Fact::Returned {
            outcome: Outcome::Observed(text),
            ..
        } => Some(text),
        _ => None,
    }
}

fn cue_text(eja: &Eja, cue: Cue) -> Result<String> {
    evidence_text(&eja.read(cue.fact)?)
        .and_then(|text| text.get(cue.start..cue.end))
        .map(str::to_owned)
        .ok_or_else(|| Error::Invalid("无效的检索线索引用".into()))
}

fn read_policy(stored: Option<&str>) -> Result<Policy> {
    Ok(serde_json::from_str(stored.unwrap_or("{}"))?)
}

fn receipt_id(entry: &Value) -> Option<Id> {
    let outcome = &entry["fact"]["outcome"];
    if entry["fact"]["kind"] == "returned"
        && outcome["status"] == "observed"
        && outcome["detail"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty())
    {
        entry["id"].as_i64()
    } else {
        None
    }
}

fn found(result: &Value) -> impl Iterator<Item = &Value> {
    result["matches"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|hit| [hit, &hit["latest"]])
}

impl HostRuntime {
    pub(super) fn search_turn(
        &mut self,
        query: &str,
        provider: &ProviderKind,
        budget: usize,
        trace: &mut SearchTurn,
    ) -> Result<Value> {
        let learnable = !query.is_empty() && query.len() <= 256 && budget > 0;
        let version = if learnable {
            read_policy(self.eja.get_meta(&key(query))?.as_deref())?.active
        } else {
            None
        };
        let through = self.eja.revision()?;
        let mut effective = query.to_owned();
        if let Some(id) = version {
            let Fact::Acted { capability, input } = self.eja.read(id)?.fact else {
                return Err(Error::Invalid("无效的检索版本".into()));
            };
            if capability != CHANGE {
                return Err(Error::Invalid("无效的检索版本".into()));
            }
            effective = cue_text(&self.eja, serde_json::from_value(input["cue"].clone())?)?;
        }
        let mut result = self.search_packet(query, &effective, through, provider, budget)?;
        if effective != query && (found(&result).next().is_none()) {
            effective = query.to_owned();
            result = self.search_packet(query, query, through, provider, budget)?;
        }
        if learnable && result["matches"].is_array() {
            trace.visits.push(Visit {
                requested: query.to_owned(),
                effective,
                version,
                through,
                budget,
                batch: trace.batch,
                seen: found(&result)
                    .filter_map(|entry| entry["id"].as_i64())
                    .collect(),
                receipts: found(&result)
                    .filter_map(receipt_id)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            });
        }
        Ok(result)
    }

    fn search_packet(
        &mut self,
        query: &str,
        effective: &str,
        through: Id,
        provider: &ProviderKind,
        budget: usize,
    ) -> Result<Value> {
        let matches = self
            .memory
            .search_at(&self.eja, effective, through)
            .map_err(|err| match err {
                eja_memory::Error::Invalid(text) => Error::Invalid(text),
                err => Error::Storage(err.to_string()),
            })?;
        let mut result = json!({"cue": query, "matches": matches});
        if effective != query {
            result["used_cue"] = json!(effective);
        }
        Ok(cap_tool_result(provider, result, budget))
    }

    pub(super) fn learn_search(
        &mut self,
        trigger: Id,
        trace: &SearchTurn,
        answer: &str,
        provider: &ProviderKind,
    ) -> Result<()> {
        let Some(root) = trace.visits.first() else {
            return Ok(());
        };
        let mut witnesses = answer
            .split("[#")
            .skip(1)
            .filter_map(|part| part.split_once(']')?.0.parse::<Id>().ok())
            .filter(|id| *id <= root.through && !root.seen.contains(id))
            .filter_map(|id| {
                trace
                    .visits
                    .iter()
                    .find(|visit| visit.batch > root.batch && visit.receipts.contains(&id))
                    .map(|visit| (id, visit))
            });
        let Some((witness, followup)) = witnesses.next() else {
            return Ok(());
        };
        if witnesses.any(|(id, _)| id != witness) {
            return Ok(());
        }
        let Fact::Returned {
            action,
            outcome: Outcome::Observed(_),
        } = self.eja.read(witness)?.fact
        else {
            return Ok(());
        };
        let case = Case {
            trigger,
            through: root.through,
            budget: root.budget,
            provider: provider.clone(),
            witness,
            action,
            retained: root.receipts.clone(),
        };
        let query = &root.requested;
        let key = key(query);
        let stored = self.eja.get_meta(&key)?;
        let mut policy = read_policy(stored.as_deref())?;
        if policy.active != root.version {
            return Ok(());
        }
        if let Some(candidate) = policy.pending.take() {
            if candidate.case.action == action || candidate.case.trigger >= trigger {
                return Ok(());
            }
            let cue = cue_text(&self.eja, candidate.cue)?;
            if self.reaches(query, &cue, &candidate.case)? && self.reaches(query, &cue, &case)? {
                return self.eja.transaction(|tx| {
                    if tx.get_meta(&key)? != stored {
                        return Ok(());
                    }
                    let call = tx.act(
                        CHANGE,
                        json!({
                            "key": key, "parent": policy.active, "cue": candidate.cue,
                            "cases": [candidate.case, case],
                        }),
                    )?;
                    tx.finish(call.id, Outcome::Observed(String::new()))?;
                    policy.active = Some(call.id);
                    tx.set_meta(&key, &serde_json::to_string(&policy)?)
                });
            }
        }
        let evidence = root
            .seen
            .iter()
            .map(|&id| self.eja.read(id))
            .collect::<Result<Vec<_>>>()?;
        let followup = followup.effective.as_str();
        let mut terms: Vec<_> = std::iter::once(followup)
            .chain(
                followup.split(|ch: char| ch.is_whitespace() || ",，、。:：；「」\"".contains(ch)),
            )
            .filter(|term| {
                *term != root.effective
                    && term.chars().count() >= 2
                    && !term.chars().any(char::is_control)
            })
            .collect();
        terms.sort_unstable_by_key(|term| (Reverse(term.chars().count()), *term));
        terms.dedup();
        for (term, cue) in terms
            .into_iter()
            .filter_map(|term| {
                evidence
                    .iter()
                    .find_map(|entry| {
                        evidence_text(entry)?.find(term).map(|start| Cue {
                            fact: entry.id,
                            start,
                            end: start + term.len(),
                        })
                    })
                    .map(|cue| (term, cue))
            })
            .take(4)
        {
            if self.reaches(query, term, &case)? {
                policy.pending = Some(Candidate { cue, case });
                break;
            }
        }
        let value = serde_json::to_string(&policy)?;
        if stored.as_deref() == Some(&value) || (stored.is_none() && policy.pending.is_none()) {
            return Ok(());
        }
        self.eja.transaction(|tx| {
            if tx.get_meta(&key)? == stored {
                tx.set_meta(&key, &value)?;
            }
            Ok(())
        })
    }

    fn reaches(&mut self, query: &str, cue: &str, case: &Case) -> Result<bool> {
        let result = self.search_packet(query, cue, case.through, &case.provider, case.budget)?;
        let receipts: Vec<_> = found(&result).filter_map(receipt_id).collect();
        Ok(
            receipts.contains(&case.witness)
                && case.retained.iter().all(|id| receipts.contains(id)),
        )
    }
}
