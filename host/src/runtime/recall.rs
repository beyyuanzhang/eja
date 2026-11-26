use crate::{
    config::ProviderKind,
    llm::{cap_tool_result, result_fits},
};
use eja_core::{Eja, Entry, Error, Fact, Id, Outcome, Result, Source};
use eja_memory::MemoryIndex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{cmp::Reverse, collections::BTreeSet};

pub(super) const CHANGE: &str = "eja.recall";

#[cfg(test)]
mod tests;

pub(super) struct SearchTurn {
    policy_head: Id,
    pub batch: usize,
    visits: Vec<Visit>,
    stale: BTreeSet<(String, Id)>,
}

impl SearchTurn {
    pub(super) fn new(policy_head: Id) -> Self {
        Self {
            policy_head,
            batch: 0,
            visits: Vec::new(),
            stale: BTreeSet::new(),
        }
    }

    pub(super) fn received(&self, id: Id) -> bool {
        self.visits.iter().any(|visit| visit.receipts.contains(&id))
    }
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

impl Case {
    fn covered_by(&self, packet: &Value) -> bool {
        let seen: BTreeSet<_> = receipts(packet).collect();
        seen.contains(&self.witness) && self.retained.is_subset(&seen)
    }
}

/// Route key shared by queries that differ only in case, whitespace, or punctuation.
fn key(query: &str) -> String {
    let folded: String = query
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let basis = if folded.is_empty() {
        query.trim().to_lowercase()
    } else {
        folded
    };
    format!("recall:{:x}", Sha256::digest(basis))
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

#[derive(Deserialize)]
struct Route {
    key: String,
    parent: Option<Id>,
    cue: Cue,
}

fn route_at(eja: &Eja, key: &str, head: Id) -> Result<Option<(Id, Cue)>> {
    let mut version = read_policy(eja.get_meta(key)?.as_deref())?.active;
    while let Some(id) = version {
        let Fact::Acted { capability, input } = eja.read(id)?.fact else {
            return Err(Error::Invalid("无效的检索版本".into()));
        };
        if capability != CHANGE {
            return Err(Error::Invalid("无效的检索版本".into()));
        }
        let route: Route = serde_json::from_value(input)?;
        if route.key != key
            || route
                .parent
                .is_some_and(|parent| parent <= 0 || parent >= id)
        {
            return Err(Error::Invalid("无效的检索版本沿革".into()));
        }
        if id <= head {
            return Ok(Some((id, route.cue)));
        }
        version = route.parent;
    }
    Ok(None)
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

pub(super) struct Recall {
    memory: MemoryIndex,
}

impl Recall {
    pub(super) fn new(memory: MemoryIndex) -> Self {
        Self { memory }
    }

    pub(super) fn warm(&mut self, eja: &Eja) {
        let _ = self.memory.warm(eja);
    }

    pub(super) fn search(
        &mut self,
        eja: &Eja,
        query: &str,
        provider: &ProviderKind,
        budget: usize,
        trace: Option<&mut SearchTurn>,
    ) -> Result<Value> {
        if budget == 0 {
            return Ok(Value::Null);
        }
        let through = eja.revision()?;
        let Some(trace) = trace.filter(|_| !query.is_empty() && query.len() <= 256) else {
            return self.search_packet(eja, query, query, through, provider, budget);
        };
        let key = key(query);
        let route = route_at(eja, &key, trace.policy_head)?;
        let version = route.map(|(id, _)| id);
        let effective = match route {
            Some((_, cue)) => cue_text(eja, cue)?,
            None => query.to_owned(),
        };
        let result = self.search_packet(eja, query, &effective, through, provider, budget)?;
        if result["matches"].is_array() {
            if let Some(version) =
                version.filter(|_| effective != query && result.get("used_cue").is_none())
            {
                trace.stale.insert((key, version));
            }
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

    /// Retire routes whose learned cue no longer kept this turn's original receipts.
    /// Runs after the turn so the turn's policy snapshot never changes mid-turn.
    pub(super) fn settle(&self, eja: &Eja, trace: &SearchTurn) -> Result<()> {
        for (key, version) in &trace.stale {
            eja.transaction(|tx| {
                let mut policy = read_policy(tx.get_meta(key)?.as_deref())?;
                if policy.active != Some(*version) {
                    return Ok(());
                }
                let Fact::Acted { input, .. } = tx.evidence(*version)?.entry.fact else {
                    return Err(Error::Invalid("无效的检索版本".into()));
                };
                let route: Route = serde_json::from_value(input)?;
                let call = tx.act(CHANGE, json!({"key": key, "retired": version}))?;
                tx.finish(call.id, Outcome::Observed(String::new()))?;
                policy.active = route.parent;
                tx.set_meta(key, &serde_json::to_string(&policy)?)
            })?;
        }
        Ok(())
    }

    fn pack_search(
        &mut self,
        eja: &Eja,
        query: &str,
        effective: &str,
        through: Id,
        provider: &ProviderKind,
        budget: usize,
    ) -> Result<Value> {
        let matches = self
            .memory
            .search_at(eja, effective, through)
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
    }

    fn search_packet(
        &mut self,
        eja: &Eja,
        query: &str,
        cue: &str,
        through: Id,
        provider: &ProviderKind,
        budget: usize,
    ) -> Result<Value> {
        let original = self.pack_search(eja, query, query, through, provider, budget)?;
        if cue == query {
            return Ok(original);
        }
        let learned = self.pack_search(eja, query, cue, through, provider, budget)?;
        if found(&learned).next().is_some()
            && receipts(&original).all(|id| receipts(&learned).any(|learned_id| learned_id == id))
        {
            return Ok(learned);
        }
        Ok(original)
    }

    pub(super) fn learn(
        &mut self,
        eja: &Eja,
        trigger: Id,
        trace: &SearchTurn,
        witness: Id,
        provider: &ProviderKind,
    ) -> Result<()> {
        let Some(root) = trace.visits.first() else {
            return Ok(());
        };
        let follows = |visit: &Visit, id| visit.batch > root.batch && visit.receipts.contains(&id);
        if witness > root.through
            || root.seen.contains(&witness)
            || !trace.visits.iter().any(|visit| follows(visit, witness))
        {
            return Ok(());
        }
        let Fact::Returned { action, .. } = eja.read(witness)?.fact else {
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
        let stored = eja.get_meta(&key)?;
        let mut policy = read_policy(stored.as_deref())?;
        if policy.active != root.version {
            return Ok(());
        }
        let had_pending = policy.pending.is_some();
        if let Some(candidate) = policy.pending.take() {
            if candidate.case.action == action || candidate.case.trigger >= trigger {
                return Ok(());
            }
            let cue = cue_text(eja, candidate.cue)?;
            let prior = &candidate.case;
            let replay = self.search_packet(
                eja,
                query,
                &cue,
                prior.through,
                &prior.provider,
                prior.budget,
            )?;
            if prior.covered_by(&replay) && self.reaches(eja, query, &cue, &case)? {
                return eja.transaction(|tx| {
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
            .map(|&id| eja.read(id))
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
            if self.reaches(eja, query, term, &case)? {
                policy.pending = Some(Candidate { cue, case });
                break;
            }
        }
        if !had_pending && policy.pending.is_none() {
            return Ok(());
        }
        eja.transaction(|tx| {
            if tx.get_meta(&key)? == stored {
                tx.set_meta(&key, &serde_json::to_string(&policy)?)?;
            }
            Ok(())
        })
    }

    fn reaches(&mut self, eja: &Eja, query: &str, cue: &str, case: &Case) -> Result<bool> {
        // The delivered first packet already preserves every original-query receipt.
        let packet =
            self.pack_search(eja, query, cue, case.through, &case.provider, case.budget)?;
        Ok(case.covered_by(&packet))
    }
}
