use super::HostRuntime;
use crate::{
    config::ProviderKind,
    llm::{cap_tool_result, result_fits},
};
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
    receipts: BTreeSet<Id>,
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
    retained: BTreeSet<Id>,
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
    let id = entry["id"].as_i64()?;
    let outcome = &entry["fact"]["outcome"];
    let text = outcome["detail"].as_str()?;
    (outcome["status"] == "observed" && !text.trim().is_empty()).then_some(id)
}

fn found(result: &Value) -> impl Iterator<Item = &Value> {
    result["matches"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|hit| [hit, &hit["latest"]])
}

fn receipts(result: &Value) -> impl Iterator<Item = Id> + '_ {
    found(result).filter_map(receipt_id)
}

impl HostRuntime {
    pub(super) fn search_turn(
        &mut self,
        query: &str,
        provider: &ProviderKind,
        budget: usize,
        trace: Option<&mut SearchTurn>,
    ) -> Result<Value> {
        if budget == 0 {
            return Ok(Value::Null);
        }
        let through = self.eja.revision()?;
        let Some(trace) = trace.filter(|_| !query.is_empty() && query.len() <= 256) else {
            return self.search_packet(query, query, through, provider, budget);
        };
        let version = read_policy(self.eja.get_meta(&key(query))?.as_deref())?.active;
        let effective = match version {
            Some(id) => match self.eja.read(id)?.fact {
                Fact::Acted { capability, input } if capability == CHANGE => {
                    cue_text(&self.eja, serde_json::from_value(input["cue"].clone())?)?
                }
                _ => return Err(Error::Invalid("无效的检索版本".into())),
            },
            None => query.to_owned(),
        };
        let result = self.search_packet(query, &effective, through, provider, budget)?;
        if result["matches"].is_array() {
            trace.visits.push(Visit {
                requested: query.to_owned(),
                effective: result["used_cue"].as_str().unwrap_or(query).to_owned(),
                version,
                through,
                budget,
                batch: trace.batch,
                seen: found(&result)
                    .filter_map(|entry| entry["id"].as_i64())
                    .collect(),
                receipts: receipts(&result).collect(),
            });
        }
        Ok(result)
    }

    fn search_packet(
        &mut self,
        query: &str,
        cue: &str,
        through: Id,
        provider: &ProviderKind,
        budget: usize,
    ) -> Result<Value> {
        let mut pack = |effective| -> Result<Value> {
            let matches = self
                .memory
                .search_at(&self.eja, effective, through)
                .map_err(|err| match err {
                    eja_memory::Error::Invalid(text) => Error::Invalid(text),
                    err => Error::Storage(err.to_string()),
                })?;
            let mut packed = json!({"cue": query, "matches": []});
            if effective != query {
                packed["used_cue"] = json!(effective);
            }
            let mut result = packed.clone();
            result["matches"] = json!(matches);
            if result_fits(&result.to_string(), provider, budget) {
                return Ok(result);
            }
            packed["truncated"] = json!(true);
            for hit in result["matches"].as_array().unwrap() {
                packed["matches"].as_array_mut().unwrap().push(hit.clone());
                if !result_fits(&packed.to_string(), provider, budget) {
                    packed["matches"].as_array_mut().unwrap().pop();
                }
            }
            if found(&packed).next().is_some() {
                return Ok(packed);
            }
            Ok(cap_tool_result(provider, result, budget))
        };
        let original = pack(query)?;
        if cue == query {
            return Ok(original);
        }
        let learned = pack(cue)?;
        if found(&learned).next().is_some()
            && receipts(&original).all(|id| receipts(&learned).any(|learned_id| learned_id == id))
        {
            return Ok(learned);
        }
        Ok(original)
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
        let follows = |visit: &Visit, id| visit.batch > root.batch && visit.receipts.contains(&id);
        let mut witnesses = answer
            .split("[#")
            .skip(1)
            .filter_map(|part| part.split_once(']')?.0.parse::<Id>().ok())
            .filter(|id| *id <= root.through && !root.seen.contains(id))
            .filter(|&id| trace.visits.iter().any(|visit| follows(visit, id)));
        let Some(witness) = witnesses.next() else {
            return Ok(());
        };
        if witnesses.any(|id| id != witness) {
            return Ok(());
        }
        let Fact::Returned { action, .. } = self.eja.read(witness)?.fact else {
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
        let had_pending = policy.pending.is_some();
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
        let mut terms: Vec<_> = trace
            .visits
            .iter()
            .filter(|visit| follows(visit, witness))
            .flat_map(|visit| {
                let cue = visit.effective.as_str();
                std::iter::once(cue).chain(
                    cue.split(|ch: char| ch.is_whitespace() || ",，、。:：；「」\"".contains(ch)),
                )
            })
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
        if !had_pending && policy.pending.is_none() {
            return Ok(());
        }
        self.eja.transaction(|tx| {
            if tx.get_meta(&key)? == stored {
                tx.set_meta(&key, &serde_json::to_string(&policy)?)?;
            }
            Ok(())
        })
    }

    fn reaches(&mut self, query: &str, cue: &str, case: &Case) -> Result<bool> {
        let packet = self.search_packet(query, cue, case.through, &case.provider, case.budget)?;
        let seen: BTreeSet<_> = receipts(&packet).collect();
        Ok(seen.contains(&case.witness) && case.retained.is_subset(&seen))
    }
}
