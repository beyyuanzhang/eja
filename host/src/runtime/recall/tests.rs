use super::*;
use crate::{
    config::{HostConfig, ProviderConfig},
    hands::{ToolSpec, ANSWER_WITH_RECEIPT},
    llm::{Brain, HttpBrain, ModelReply},
    runtime::HostRuntime,
    state,
};
use std::sync::{Arc, Mutex};

impl HostRuntime {
    fn search_turn(
        &mut self,
        query: &str,
        provider: &ProviderKind,
        budget: usize,
        trace: Option<&mut SearchTurn>,
    ) -> Result<Value> {
        self.recall
            .search(&self.eja, query, provider, budget, trace)
    }

    fn learn_search(
        &mut self,
        trigger: Id,
        trace: &SearchTurn,
        receipt: Id,
        provider: &ProviderKind,
    ) -> Result<()> {
        self.recall
            .learn(&self.eja, trigger, trace, receipt, provider)
    }

    fn search_trace(&self) -> SearchTurn {
        SearchTurn::new(self.eja.revision().unwrap())
    }
}

#[derive(Default)]
struct Judge {
    requests: Mutex<Vec<Vec<String>>>,
    searches: Mutex<Vec<String>>,
    answer_receipts: Mutex<Vec<Id>>,
}

impl Brain for Judge {
    fn complete(
        &self,
        provider: &ProviderConfig,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> std::result::Result<ModelReply, String> {
        assert!(!system.contains("[#123]"));
        assert!(tools.iter().any(|tool| tool.name == ANSWER_WITH_RECEIPT));
        let results = messages
            .iter()
            .flat_map(|message| {
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == "tool_result")
                    .chain(std::iter::once(message).filter(|message| message["role"] == "tool"))
            })
            .filter_map(|result| result["content"].as_str().map(str::to_owned))
            .collect();
        self.requests.lock().unwrap().push(results);
        let reply = HttpBrain.complete(provider, model, system, messages, tools)?;
        self.searches.lock().unwrap().extend(
            reply
                .calls
                .iter()
                .filter(|call| call.name == "search")
                .filter_map(|call| call.args["cue"].as_str().map(str::to_owned)),
        );
        self.answer_receipts.lock().unwrap().extend(
            reply
                .calls
                .iter()
                .filter(|call| call.name == ANSWER_WITH_RECEIPT)
                .filter_map(|call| call.args["receipt"].as_i64()),
        );
        Ok(reply)
    }

    fn test_connection(&self, _: &ProviderConfig, _: &str) -> std::result::Result<String, String> {
        unreachable!()
    }
}

fn note(rt: &HostRuntime, detail: &str) -> Id {
    let id = rt.eja.notice(Source::Person, detail).unwrap();
    state::complete_turn(&rt.eja, id, "").unwrap();
    id
}

fn receipt(rt: &HostRuntime, label: &str, outcome: Outcome) -> Id {
    let call = rt
        .eja
        .act(
            rt.eja.revision().unwrap(),
            "archive",
            json!({"case": label}),
        )
        .unwrap();
    rt.eja.finish(call.id, outcome).unwrap().id
}

fn active(rt: &HostRuntime, query: &str) -> Option<(Id, Value)> {
    let version = read_policy(rt.eja.get_meta(&key(query)).unwrap().as_deref())
        .unwrap()
        .active?;
    let Fact::Acted { capability, input } = rt.eja.read(version).unwrap().fact else {
        unreachable!()
    };
    assert_eq!(capability, CHANGE);
    let evidence = rt.eja.evidence(version).unwrap();
    assert!(matches!(
        evidence.latest.unwrap().fact,
        Fact::Returned {
            outcome: Outcome::Observed(_),
            ..
        }
    ));
    Some((version, input))
}

#[test]
fn search_keeps_complete_receipts_when_a_higher_ranked_match_overflows() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = HostRuntime::open_with_brain(
        dir.path().join("subject"),
        Arc::new(Judge::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    let id = receipt(&rt, "signed", Outcome::Observed("ROUTE signed P-A".into()));
    note(&rt, &format!("ROUTE archive note {}", "x".repeat(2000)));
    for provider in [ProviderKind::OpenAi, ProviderKind::Anthropic] {
        let mut trace = rt.search_trace();
        let packet = rt
            .search_turn("ROUTE", &provider, 512, Some(&mut trace))
            .unwrap();
        assert_eq!(packet["truncated"], true);
        assert_eq!(
            found(&packet).filter_map(receipt_id).collect::<Vec<_>>(),
            [id]
        );
        assert_eq!(trace.visits[0].receipts, BTreeSet::from([id]));
        assert!(trace.received(id));
        assert!(!trace.received(id + 1));
        assert!(rt
            .search_turn("", &provider, 0, Some(&mut trace))
            .unwrap()
            .is_null());
        assert_eq!(trace.visits.len(), 1);
    }
}

#[test]
fn independent_receipt_rejects_a_draft_route_and_commits_the_shared_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = HostRuntime::open_with_brain(
        dir.path().join("subject"),
        Arc::new(Judge::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    let directory = note(&rt, "ROOT: official tags DRAFT-LINK and SHARED-LINK.");
    let first = receipt(
        &rt,
        "first",
        Outcome::Observed("first approved DRAFT-LINK SHARED-LINK".into()),
    );
    let second = receipt(
        &rt,
        "second",
        Outcome::Observed("second approved SHARED-LINK".into()),
    );
    let draft = receipt(&rt, "draft", Outcome::Failed("draft DRAFT-LINK".into()));
    let provider = ProviderKind::OpenAi;
    let run = |rt: &mut HostRuntime, query: &str, cited: Id, independent_batch: bool| {
        let trigger = note(rt, "check the signed record");
        let mut trace = rt.search_trace();
        rt.search_turn("ROOT", &provider, 4096, Some(&mut trace))
            .unwrap();
        trace.batch = usize::from(independent_batch);
        rt.search_turn(query, &provider, 4096, Some(&mut trace))
            .unwrap();
        rt.learn_search(trigger, &trace, cited, &provider).unwrap();
    };
    let stored =
        |rt: &HostRuntime| read_policy(rt.eja.get_meta(&key("ROOT")).unwrap().as_deref()).unwrap();
    run(&mut rt, "DRAFT-LINK", first, false);
    assert!(stored(&rt).pending.is_none());
    run(&mut rt, "DRAFT-LINK", draft, true);
    assert!(stored(&rt).pending.is_none());
    run(&mut rt, "DRAFT-LINK", first, true);
    run(&mut rt, "DRAFT-LINK", first, true);
    assert!(active(&rt, "ROOT").is_none());
    run(&mut rt, "SHARED-LINK", second, true);
    assert!(active(&rt, "ROOT").is_none());
    let pending = stored(&rt).pending.unwrap();
    assert_eq!(cue_text(&rt.eja, pending.cue).unwrap(), "SHARED-LINK");
    let mut in_flight = rt.search_trace();
    run(&mut rt, "SHARED-LINK", first, true);
    let packet = rt
        .search_turn("ROOT", &provider, 4096, Some(&mut in_flight))
        .unwrap();
    assert!(packet.get("used_cue").is_none());
    assert_eq!(in_flight.visits[0].version, None);
    let (_, change) = active(&rt, "ROOT").unwrap();
    assert_eq!(change["cue"]["fact"], directory);
    assert!(change["parent"].is_null());
    drop(rt);

    let mut rt = HostRuntime::open_with_brain(
        dir.path().join("subject"),
        Arc::new(Judge::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    let mut trace = rt.search_trace();
    let unrelated = rt
        .search_turn("unrelated", &provider, 4096, Some(&mut trace))
        .unwrap();
    assert!(unrelated.get("used_cue").is_none());
    let result = rt
        .search_turn("ROOT", &provider, 4096, Some(&mut trace))
        .unwrap();
    assert_eq!(result["used_cue"], "SHARED-LINK");
    assert_eq!(found(&result).filter_map(receipt_id).count(), 2);
    let disabled = rt.search_turn("ROOT", &provider, 4096, None).unwrap();
    assert!(disabled.get("used_cue").is_none() && receipts(&disabled).next().is_none());

    let new = receipt(
        &rt,
        "root-only",
        Outcome::Observed("ROOT signed P-NEW".into()),
    );
    let result = rt
        .search_turn("ROOT", &provider, 4096, Some(&mut trace))
        .unwrap();
    assert_eq!(
        result,
        rt.search_turn("ROOT", &provider, 4096, None).unwrap()
    );
    assert!(receipts(&result).any(|id| id == new));
    assert_eq!(active(&rt, "ROOT").unwrap().1, change);
}

#[test]
#[ignore = "requires EJA_RECALL_LEARNING=1 and a live model configured by EJA_EVAL_CONFIG"]
fn search_live_rsi_from_identity() {
    assert_eq!(std::env::var("EJA_RECALL_LEARNING").as_deref(), Ok("1"));
    let path = std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG");
    let mut config: HostConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    config.capabilities.clear();
    config.window_max_bytes = Some(8192);
    config.max_steps = Some(8);
    println!("model={}", config.resolve_judge().unwrap().1);
    run_rsi_lineage(Some(&config));
}

#[test]
fn search_replay_rsi_from_identity() {
    run_rsi_lineage(None);
}

#[cfg(feature = "onnx")]
#[test]
#[ignore = "requires the local model directory in EJA_ONNX_MODEL_DIR"]
fn search_replay_rsi_with_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = HostRuntime::open_with_brain(
        dir.path().join("subject"),
        Arc::new(Judge::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    assert!(rt.core_dir.path().join("memory.db").is_file());
    let root = "礼堂场地预约";
    let cue = "停电药品冷藏柜";
    note(&rt, &format!("{root}索引。{root}签收回执请查 {cue}。"));
    for i in 1..=14 {
        note(&rt, &format!("{root}第 {i} 次，核对演讲厅排期和座位。"));
    }
    let search = |rt: &mut HostRuntime, query: &str, trace: Option<&mut SearchTurn>| {
        rt.search_turn(query, &ProviderKind::OpenAi, 8192, trace)
            .unwrap()
    };
    for code in ["P-FIRST", "P-SECOND"] {
        let id = receipt(
            &rt,
            code,
            Outcome::Observed(format!("{cue}中胰岛素交接号 {code}。")),
        );
        let trigger = note(&rt, "核验签收记录");
        let mut trace = rt.search_trace();
        let first = search(&mut rt, root, Some(&mut trace));
        assert!(first.to_string().contains(cue));
        assert!(!receipts(&first).any(|seen| seen == id));
        trace.batch = 1;
        let followup = search(&mut rt, cue, Some(&mut trace));
        assert!(receipts(&followup).any(|seen| seen == id));
        rt.learn_search(trigger, &trace, id, &ProviderKind::OpenAi)
            .unwrap();
    }
    let id = receipt(
        &rt,
        "新交接",
        Outcome::Observed("供电中断后，胰岛素转到邻居的冰箱保鲜，原始交接号 P-THIRD。".into()),
    );
    assert!(eja_memory::MemoryIndex::keyword()
        .search(&rt.eja, cue)
        .unwrap()
        .iter()
        .all(|hit| hit.evidence.entry.id != id));
    let mut trace = rt.search_trace();
    let learned = search(&mut rt, root, Some(&mut trace));
    assert_eq!(learned["used_cue"], cue);
    assert!(receipts(&learned).any(|seen| seen == id));
    assert!(!receipts(&search(&mut rt, root, None)).any(|seen| seen == id));
    assert!(receipts(&search(&mut rt, cue, None)).any(|seen| seen == id));
    println!("vector-only held-out receipt {id} reached by ordinary search and inherited cue");
}

fn run_rsi_lineage(config: Option<&HostConfig>) {
    let seed = std::env::var("EJA_RSL_TEST_SEED").unwrap_or_else(|_| "20261126".into());
    let opaque = |label: &str| {
        let digest = format!("{:x}", Sha256::digest(format!("{seed}:{label}")));
        digest[..10].to_uppercase()
    };
    let root = format!("IDX-{}", opaque("root"));
    // Each narrower archive tag includes its successor, so the successor can
    // retrieve earlier receipts. The explicit next link arrives in a receipt.
    let mut routes = vec![format!("ARCH-{}", opaque("archive"))];
    for index in 1..4 {
        routes.push(format!(
            "{}-{}",
            routes[index - 1],
            opaque(&index.to_string())
        ));
    }
    routes.reverse();

    let provider = config
        .map(|config| config.resolve_judge().unwrap().0.kind.clone())
        .unwrap_or_default();
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| {
        let judge = Arc::new(Judge::default());
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join(name),
            judge.clone(),
            Arc::new(move |event| {
                if event["event"] == "error" {
                    sink.lock().unwrap().push(event["message"].clone());
                }
            }),
        )
        .unwrap();
        if let Some(config) = config {
            rt.save_config_request(config.clone().into(), None, false);
        }
        (rt, judge, errors)
    };
    let (mut rt, mut judge, mut errors) = open("learned");
    note(
        &rt,
        &format!(
            "场地预约的归档入口 {root}。第一批正式签发簿的完整档案标签为 {}。",
            routes[0]
        ),
    );
    let question = |name: &str, generation: usize| {
        format!(
            "核验第 {generation} 批{name}的场地预约。首次 search 的 cue 只填完整入口 {root}。看到其他档案标签时同样原样搜索。看到目标凭证和对应回执 ID 后，用 answer_with_receipt 提交，text 填凭证码，receipt 填整数 ID。"
        )
    };
    let add_receipt = |rt: &HostRuntime, name: &str, index: usize, link: bool| {
        let code = format!("P-{}", opaque(name));
        let next = routes
            .get(index + 1)
            .filter(|_| link)
            .map(|cue| format!("第 {} 批正式签发簿的完整档案标签为 {cue}。", index + 2))
            .unwrap_or_default();
        let id = receipt(
            rt,
            name,
            Outcome::Observed(format!(
                "第 {} 批{}，正式签发簿档案标签 {}。正式凭证码 {code}。{next}",
                index + 1,
                name,
                routes[index],
            )),
        );
        (id, code)
    };
    let send = |rt: &mut HostRuntime,
                prompt: &str,
                id: Id,
                code: &str,
                judge: &Judge,
                errors: &Mutex<Vec<Value>>| {
        note(rt, &"日常记录。".repeat(570));
        let cursor = rt.eja.revision().unwrap();
        let start = judge.requests.lock().unwrap().len();
        let search_start = judge.searches.lock().unwrap().len();
        let answer_start = judge.answer_receipts.lock().unwrap().len();
        rt.handle_user_message(prompt);
        let answer = rt
            .eja
            .entries_after(cursor, rt.eja.revision().unwrap())
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.fact {
                Fact::Said { text } => Some(text),
                _ => None,
            });
        let requests = judge.requests.lock().unwrap();
        let searches = judge.searches.lock().unwrap();
        let receipt_call = requests[start..]
            .iter()
            .position(|results| {
                results.iter().any(|result| {
                    let packet: Value = serde_json::from_str(result).unwrap();
                    let visible = found(&packet).any(|entry| {
                        receipt_id(entry) == Some(id)
                            && entry["fact"]["outcome"]["detail"]
                                .as_str()
                                .is_some_and(|detail| detail.contains(code))
                    });
                    visible
                })
            })
            .map(|index| index + 1);
        let errors = errors.lock().unwrap();
        let answer_receipts = judge.answer_receipts.lock().unwrap();
        println!("receipt={id} receipt_call={receipt_call:?} calls={} searches={:?} answer={answer:?} selected={:?} errors={errors:?}",
            requests.len() - start, &searches[search_start..], &answer_receipts[answer_start..]);
        assert!(errors.iter().all(|message| {
            message
                .as_str()
                .is_some_and(|message| message == "达到单回合最大步数上限 (8)，回合已暂停")
        }));
        let correct = answer.is_some_and(|answer| answer.contains(code))
            && answer_receipts[answer_start..].contains(&id);
        (receipt_call, requests.len() - start, correct)
    };

    let visible =
        |packet: &Value, id: Id| found(packet).filter_map(receipt_id).any(|seen| seen == id);
    let mut parent = None;
    let mut confirmed = Vec::new();
    for (index, names) in [
        ["北厅", "西厅"],
        ["东厅", "南厅"],
        ["前厅", "后厅"],
        ["左厅", "右厅"],
    ]
    .into_iter()
    .enumerate()
    {
        let mut in_flight = rt.search_trace();
        let mut witnesses = Vec::new();
        for (which, name) in names.into_iter().enumerate() {
            let (id, code) = add_receipt(&rt, name, index, which == 1);
            let prompt = question(name, index + 1);
            let first = rt
                .search_turn(&root, &provider, 6000, Some(&mut rt.search_trace()))
                .unwrap();
            assert!(
                !visible(&first, id),
                "training receipt {id} must need a follow-up"
            );
            let label = format!("标签为 {}。", routes[index]);
            assert!(
                first.to_string().contains(&label),
                "missing the explicit archive link"
            );
            if index > 0 {
                let identity = rt.search_turn(&root, &provider, 6000, None).unwrap();
                assert!(
                    !identity.to_string().contains(&label),
                    "the original query exposed the next link"
                );
            }
            if config.is_some() {
                let (first_receipt, _, correct) =
                    send(&mut rt, &prompt, id, &code, &judge, &errors);
                assert!(first_receipt.is_some() && correct);
            } else {
                let trigger = note(&rt, &prompt);
                let mut trace = rt.search_trace();
                rt.search_turn(&root, &provider, 6000, Some(&mut trace))
                    .unwrap();
                trace.batch = 1;
                if index == 1 {
                    let broad = rt
                        .search_turn("正式凭证码", &provider, 6000, Some(&mut trace))
                        .unwrap();
                    assert!(visible(&broad, id));
                    trace.batch += 1;
                }
                let second = rt
                    .search_turn(&routes[index], &provider, 6000, Some(&mut trace))
                    .unwrap();
                assert!(visible(&second, id));
                rt.learn_search(trigger, &trace, id, &provider).unwrap();
            }
            witnesses.push(id);
            if which == 0 {
                assert_eq!(active(&rt, &root).map(|(id, _)| id), parent);
            }
        }
        let (version, change) =
            active(&rt, &root).expect("the two independent receipts must update the route");
        assert_eq!(change["parent"], json!(parent));
        rt.search_turn(&root, &provider, 6000, Some(&mut in_flight))
            .unwrap();
        assert_eq!(in_flight.visits[0].version, parent);
        assert_eq!(
            cue_text(
                &rt.eja,
                serde_json::from_value(change["cue"].clone()).unwrap()
            )
            .unwrap(),
            routes[index]
        );
        assert_eq!(change["cases"][0]["witness"], witnesses[0]);
        assert_eq!(change["cases"][1]["witness"], witnesses[1]);
        let post = rt
            .search_turn(&root, &provider, 6000, Some(&mut rt.search_trace()))
            .unwrap();
        confirmed.extend(witnesses.iter().copied());
        assert!(
            confirmed.iter().all(|&id| visible(&post, id)),
            "past receipts regressed"
        );
        println!("generation={} version={version} parent={parent:?} witnesses={witnesses:?} retained={}/{}",
            index + 1, confirmed.len(), confirmed.len());
        parent = Some(version);
        drop(rt);
        (rt, judge, errors) = open("learned");
        assert_eq!(active(&rt, &root).map(|(id, _)| id), parent);
    }

    let (id, code) = add_receipt(&rt, "中庭", 3, false);
    let inherited = rt
        .search_turn(&root, &provider, 6000, Some(&mut rt.search_trace()))
        .unwrap();
    let identity = rt.search_turn(&root, &provider, 6000, None).unwrap();
    assert!(visible(&inherited, id));
    assert!(!visible(&identity, id));
    println!(
        "holdout={id} first_search inherited=true identity=false retained={}",
        confirmed
            .iter()
            .filter(|&&id| visible(&inherited, id))
            .count()
    );
    if config.is_some() {
        let prompt = question("中庭", 4);
        // Fork the same journal before either arm adds its holdout question.
        let learned_db = rt.core_dir.path().join("subject.db");
        let (frozen, _, _) = open("frozen");
        let frozen_db = frozen.core_dir.path().join("subject.db");
        drop(rt);
        drop(frozen);
        std::fs::copy(learned_db, frozen_db).unwrap();
        (rt, judge, errors) = open("learned");
        let learned = send(&mut rt, &prompt, id, &code, &judge, &errors);
        let (mut frozen_rt, judge, errors) = open("frozen");
        frozen_rt.eja.set_meta(&key(&root), "{}").unwrap();
        let frozen = send(&mut frozen_rt, &prompt, id, &code, &judge, &errors);
        println!(
            "holdout (receipt_call, total_calls, correct) learned={learned:?} frozen={frozen:?}"
        );
        assert_eq!(learned.0, Some(2));
        assert!(learned.2);
        assert!(frozen.0.is_none_or(|call| call > 2));
        assert!(active(&frozen_rt, &root).is_none());
        return;
    }

    let alternative = "SIDE-ARCHIVE";
    note(&rt, &format!("{} 另一路索引 {alternative}。", routes[3]));
    for name in ["耳房", "外厅"] {
        let id = receipt(
            &rt,
            name,
            Outcome::Observed(format!("{alternative} {name}的正式签发回执。")),
        );
        let trigger = note(&rt, "核对另一路的正式回执");
        let mut trace = rt.search_trace();
        let first = rt
            .search_turn(&root, &provider, 6000, Some(&mut trace))
            .unwrap();
        assert!(first.to_string().contains(alternative));
        assert!(confirmed.iter().all(|&id| visible(&first, id)));
        trace.batch = 1;
        let second = rt
            .search_turn(alternative, &provider, 6000, Some(&mut trace))
            .unwrap();
        assert!(visible(&second, id) && confirmed.iter().all(|&id| !visible(&second, id)));
        rt.learn_search(trigger, &trace, id, &provider).unwrap();
    }
    let policy = read_policy(rt.eja.get_meta(&key(&root)).unwrap().as_deref()).unwrap();
    assert_eq!(policy.active, parent);
    assert!(policy.pending.is_none());
    println!("side route rejected: each new receipt found, earlier receipts lost");

    let latest: Vec<_> = ["一厅", "二厅", "三厅", "四厅"]
        .into_iter()
        .map(|name| add_receipt(&rt, name, 3, false).0)
        .collect();
    let crowded = rt
        .search_turn(&root, &provider, 6000, Some(&mut rt.search_trace()))
        .unwrap();
    assert!(latest.iter().all(|&id| visible(&crowded, id)));
    let retained = confirmed
        .iter()
        .filter(|&&id| visible(&crowded, id))
        .count();
    assert!(retained < confirmed.len());
    assert_eq!(active(&rt, &root).map(|(id, _)| id), parent);
    println!(
        "same route after journal growth: earlier receipts={retained}/{}",
        confirmed.len()
    );
}

#[test]
fn search_replay_inherited_policy_helps_next_update() {
    let provider = ProviderKind::OpenAi;
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| {
        HostRuntime::open_with_brain(
            dir.path().join(name),
            Arc::new(Judge::default()),
            Arc::new(|_| {}),
        )
        .unwrap()
    };
    let mut inherited = open("inherited");
    let root = "IDX-2E47A808";
    let local = "LOCAL-1F5C49";
    let central = "CENTRAL-90D738";
    note(
        &inherited,
        &format!("归档入口 {root}。地方签发簿索引 {local}。"),
    );
    for name in ["A", "B"] {
        let id = receipt(
            &inherited,
            name,
            Outcome::Observed(format!("{local}；共享索引 {central}；旧回执 {name}。")),
        );
        let trigger = note(&inherited, "核验前批回执");
        let mut trace = inherited.search_trace();
        inherited
            .search_turn(root, &provider, 4096, Some(&mut trace))
            .unwrap();
        trace.batch = 1;
        inherited
            .search_turn(local, &provider, 4096, Some(&mut trace))
            .unwrap();
        inherited
            .learn_search(trigger, &trace, id, &provider)
            .unwrap();
    }
    let (parent, _) = active(&inherited, root).unwrap();
    note(
        &inherited,
        &format!("地方签发簿 {local}。本批两厅由中央签发簿签发，档案索引 {central}。"),
    );
    let cases = [("北厅", "P-741A98B6"), ("西厅", "P-23F80CEC")];
    let expected: Vec<_> = cases
        .iter()
        .map(|(name, code)| {
            receipt(
                &inherited,
                name,
                Outcome::Observed(format!("中央签发簿 {central}，{name}正式凭证码 {code}。")),
            )
        })
        .collect();

    let identity = open("identity");
    let source = inherited.core_dir.path().join("subject.db");
    let copy = identity.core_dir.path().join("subject.db");
    drop(inherited);
    drop(identity);
    std::fs::copy(source, copy).unwrap();
    let mut inherited = open("inherited");
    let mut identity = open("identity");
    identity.eja.set_meta(&key(root), "{}").unwrap();
    for (rt, has_link) in [(&mut inherited, true), (&mut identity, false)] {
        let result = rt
            .search_turn(root, &provider, 4096, Some(&mut rt.search_trace()))
            .unwrap();
        assert_eq!(result.to_string().contains(central), has_link);
        assert!(found(&result)
            .filter_map(receipt_id)
            .all(|id| !expected.contains(&id)));
    }

    for &id in &expected {
        for (rt, learned) in [(&mut inherited, true), (&mut identity, false)] {
            let trigger = note(rt, "核验本批回执");
            let mut trace = rt.search_trace();
            let first = rt
                .search_turn(root, &provider, 4096, Some(&mut trace))
                .unwrap();
            let cue = if first.to_string().contains(central) {
                central
            } else {
                local
            };
            trace.batch = 1;
            let second = rt
                .search_turn(cue, &provider, 4096, Some(&mut trace))
                .unwrap();
            let visible = receipts(&second).any(|seen| seen == id);
            assert_eq!(visible, learned);
            if visible {
                rt.learn_search(trigger, &trace, id, &provider).unwrap();
            }
        }
    }
    let (_, update) = active(&inherited, root).expect("expected an inherited-policy update");
    assert_eq!(update["parent"], parent);
    let proven: Vec<_> = update["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["witness"].as_i64().unwrap())
        .collect();
    assert_eq!(proven, expected);
    assert!(active(&identity, root).is_none());

    let holdout = Outcome::Observed(format!("中央签发簿 {central}，南厅正式凭证码 P-65EC9FC2。"));
    for (rt, should_see) in [(&mut inherited, true), (&mut identity, false)] {
        let id = receipt(rt, "南厅", holdout.clone());
        let result = rt
            .search_turn(root, &provider, 4096, Some(&mut rt.search_trace()))
            .unwrap();
        assert_eq!(
            found(&result).filter_map(receipt_id).any(|seen| seen == id),
            should_see
        );
    }
    println!("next_updates inherited=1 identity=0 heldout_recall inherited=true identity=false");
}
