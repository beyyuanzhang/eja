use super::*;
use crate::{
    config::{HostConfig, ProviderConfig},
    hands::ToolSpec,
    llm::{Brain, HttpBrain, ModelReply},
    state,
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Judge {
    requests: Mutex<Vec<Vec<String>>>,
    searches: Mutex<Vec<String>>,
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
        let mut trace = SearchTurn::default();
        rt.search_turn("ROOT", &provider, 4096, &mut trace).unwrap();
        trace.batch = usize::from(independent_batch);
        rt.search_turn(query, &provider, 4096, &mut trace).unwrap();
        rt.learn_search(
            trigger,
            &trace,
            &format!("signed [#{cited}] from directory [#{directory}]"),
            &provider,
        )
        .unwrap();
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
    run(&mut rt, "SHARED-LINK", first, true);
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
    let mut trace = SearchTurn::default();
    let unrelated = rt
        .search_turn("unrelated", &provider, 4096, &mut trace)
        .unwrap();
    assert!(unrelated.get("used_cue").is_none());
    let result = rt.search_turn("ROOT", &provider, 4096, &mut trace).unwrap();
    assert_eq!(result["used_cue"], "SHARED-LINK");
    assert_eq!(found(&result).filter_map(receipt_id).count(), 2);
}

#[test]
#[ignore = "requires a live model configured by EJA_EVAL_CONFIG"]
fn search_live_rsi_from_identity() {
    let path = std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG");
    let mut config: HostConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    config.capabilities.clear();
    config.window_max_bytes = Some(16384);
    config.max_steps = Some(12);
    let seed = std::env::var("EJA_RSL_TEST_SEED").unwrap_or_else(|_| "20261125".into());
    let opaque = |label: &str| {
        let digest = format!("{:x}", Sha256::digest(format!("{seed}:{label}")));
        format!("{}-{}", label, digest[..10].to_uppercase())
    };
    let (root, local, central, draft) = (
        opaque("IDX"),
        opaque("LOCAL"),
        opaque("CENTRAL"),
        opaque("DRAFT"),
    );
    println!("model={} seed={seed}", config.resolve_judge().unwrap().1);
    let dir = tempfile::tempdir().unwrap();
    let create = |name: &str| {
        let judge = Arc::new(Judge::default());
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        let mut rt = HostRuntime::open_with_brain(
            dir.path().join(name),
            judge.clone(),
            Arc::new(move |event| {
                if event["event"] == "error" {
                    sink.lock().unwrap().push(event);
                }
            }),
        )
        .unwrap();
        rt.save_config_request(config.clone().into(), None, false);
        (rt, judge, errors)
    };
    let (mut learning, judge, errors) = create("learning");
    note(
        &learning,
        &format!("场地预约归档入口 {root}。地方正式签发簿索引 {local}。草稿索引 {draft}。"),
    );
    let cases = [
        ("北厅", "地方正式签发簿"),
        ("西厅", "地方正式签发簿"),
        ("东厅", "中央正式签发簿"),
        ("南厅", "中央正式签发簿"),
        ("中庭", "中央正式签发簿"),
    ];
    let codes: Vec<_> = cases
        .iter()
        .map(|(name, _)| opaque(&format!("P{name}")))
        .collect();
    let archive = |rt: &HostRuntime, index: usize| {
        let (name, archive) = cases[index];
        let tags = if index < 2 {
            format!("{local}，共享归档键 {central}")
        } else {
            central.clone()
        };
        receipt(
            rt,
            name,
            Outcome::Observed(format!(
                "{name}，{archive}，索引 {tags}。正式签发凭证码 {}。",
                codes[index]
            )),
        )
    };
    let send = |rt: &mut HostRuntime, prompt: &str, errors: &Mutex<Vec<Value>>| {
        note(rt, &"无关历史条目占据常规窗口。".repeat(720));
        let cursor = rt.eja.revision().unwrap();
        rt.handle_user_message(prompt);
        let errors = errors.lock().unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        rt.eja
            .entries_after(cursor, rt.eja.revision().unwrap())
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.fact {
                Fact::Said { text } => Some(text),
                _ => None,
            })
            .expect("the current turn must answer")
    };
    let question = |index: usize| {
        format!(
            "这次只核对{}的场地预约。请先从归档入口 {root} 查起，核对{}上的正式凭证码，附原始回执编号。",
            cases[index].0, cases[index].1
        )
    };
    let first_receipts: Vec<_> = (0..2).map(|index| archive(&learning, index)).collect();
    receipt(
        &learning,
        "侧厅",
        Outcome::Failed(format!("侧厅 {draft}，草稿预填 P-DRAFT，尚未正式签发。")),
    );
    let check = |rt: &mut HostRuntime, index: usize, receipt: Id| {
        let start = judge.searches.lock().unwrap().len();
        let answer = send(rt, &question(index), &errors);
        println!(
            "case={} receipt={receipt} answer={answer:?}",
            cases[index].0
        );
        let searches = judge.searches.lock().unwrap();
        println!(
            "searches={:?} policy={:?}",
            &searches[start..],
            rt.eja.get_meta(&key(&root)).unwrap()
        );
        assert!(
            answer.contains(&codes[index]) && answer.contains(&format!("[#{receipt}]")),
            "{answer}"
        );
    };
    check(&mut learning, 0, first_receipts[0]);
    assert!(active(&learning, &root).is_none());
    check(&mut learning, 1, first_receipts[1]);
    let (first, route) = active(&learning, &root).expect("first autonomous update");
    assert!(route["parent"].is_null());
    assert_eq!(
        cue_text(
            &learning.eja,
            serde_json::from_value(route["cue"].clone()).unwrap()
        )
        .unwrap(),
        local
    );
    println!("generation1 action={first} evidence={}", route["cases"]);

    note(
        &learning,
        &format!("地方签发簿索引 {local}。第二批场地由中央正式签发簿出具凭证，源索引 {central}。"),
    );
    let later_receipts: Vec<_> = (2..4).map(|index| archive(&learning, index)).collect();
    check(&mut learning, 2, later_receipts[0]);
    assert_eq!(active(&learning, &root).unwrap().0, first);
    check(&mut learning, 3, later_receipts[1]);
    let (second, route) = active(&learning, &root).expect("second autonomous update");
    assert_eq!(route["parent"], first);
    assert_eq!(
        cue_text(
            &learning.eja,
            serde_json::from_value(route["cue"].clone()).unwrap()
        )
        .unwrap(),
        central
    );
    println!(
        "generation2 action={second} parent={first} evidence={}",
        route["cases"]
    );

    let held_out = archive(&learning, 4);
    let (frozen, _, _) = create("frozen");
    let learned_db = learning.core_dir.path().join("subject.db");
    let frozen_db = frozen.core_dir.path().join("subject.db");
    drop(learning);
    drop(frozen);
    std::fs::copy(learned_db, frozen_db).unwrap();
    let (mut learning, judge, errors) = create("learning");
    let (mut frozen, frozen_judge, frozen_errors) = create("frozen");
    frozen.eja.set_meta(&key(&root), "{}").unwrap();
    assert_eq!(active(&learning, &root).unwrap().0, second);
    let learned_answer = send(&mut learning, &question(4), &errors);
    let frozen_answer = send(&mut frozen, &question(4), &frozen_errors);
    let earliest_receipt = |judge: &Judge| {
        let requests = judge.requests.lock().unwrap();
        let first_visible = requests
            .iter()
            .position(|results| results.iter().any(|result| result.contains(&codes[4])))
            .expect("the original receipt must have been shown")
            + 1;
        (first_visible, requests.len())
    };
    let (learned_visible, learned_calls) = earliest_receipt(&judge);
    let (frozen_visible, frozen_calls) = earliest_receipt(&frozen_judge);
    println!("holdout learned_visible={learned_visible} frozen_visible={frozen_visible} learned_calls={learned_calls} frozen_calls={frozen_calls} learned={learned_answer:?} frozen={frozen_answer:?}");
    assert!(learned_answer.contains(&codes[4]) && frozen_answer.contains(&codes[4]));
    assert!(
        learned_answer.contains(&format!("[#{held_out}]"))
            && frozen_answer.contains(&format!("[#{held_out}]"))
    );
    assert_eq!(learned_visible, 2);
    assert!(learned_visible < frozen_visible);
    assert!(active(&frozen, &root).is_none());
}

#[test]
#[ignore = "requires a live model configured by EJA_EVAL_CONFIG"]
fn search_live_inherited_policy_helps_next_update() {
    let path = std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG");
    let mut config: HostConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    config.capabilities.clear();
    config.window_max_bytes = Some(8192);
    config.max_steps = Some(RELAY_STEPS);
    println!(
        "model={} max_calls_per_case={RELAY_STEPS}",
        config.resolve_judge().unwrap().1
    );
    compare_inherited_policy(Some(&config));
}

#[test]
fn search_replay_inherited_policy_helps_next_update() {
    compare_inherited_policy(None);
}

const RELAY_STEPS: usize = 3;

fn compare_inherited_policy(config: Option<&HostConfig>) {
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
                    sink.lock().unwrap().push(event);
                }
            }),
        )
        .unwrap();
        if let Some(config) = config {
            rt.save_config_request(config.clone().into(), None, false);
        }
        (rt, judge, errors)
    };
    let (mut inherited, _, _) = open("inherited");
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
        let mut trace = SearchTurn::default();
        inherited
            .search_turn(root, &provider, 4096, &mut trace)
            .unwrap();
        trace.batch = 1;
        inherited
            .search_turn(local, &provider, 4096, &mut trace)
            .unwrap();
        inherited
            .learn_search(trigger, &trace, &format!("核验成功 [#{id}]"), &provider)
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

    let (identity, _, _) = open("identity");
    let source = inherited.core_dir.path().join("subject.db");
    let copy = identity.core_dir.path().join("subject.db");
    drop(inherited);
    drop(identity);
    std::fs::copy(source, copy).unwrap();
    let (mut inherited, inherited_judge, inherited_errors) = open("inherited");
    let (mut identity, identity_judge, identity_errors) = open("identity");
    identity.eja.set_meta(&key(root), "{}").unwrap();
    for (rt, has_link) in [(&mut inherited, true), (&mut identity, false)] {
        let result = rt
            .search_turn(root, &provider, 4096, &mut SearchTurn::default())
            .unwrap();
        assert_eq!(result.to_string().contains(central), has_link);
        assert!(found(&result)
            .filter_map(receipt_id)
            .all(|id| !expected.contains(&id)));
    }

    for (index, (name, _)) in cases.iter().enumerate() {
        let prompt = format!("这次只核对{name}，请从归档入口 {root} 查起。看到中央签发簿的原始凭证码后，直接给出凭证码和回执编号。");
        for (arm, rt, judge) in [
            ("inherited", &mut inherited, &inherited_judge),
            ("identity", &mut identity, &identity_judge),
        ] {
            if config.is_none() {
                let trigger = note(rt, &prompt);
                let mut trace = SearchTurn::default();
                let first = rt.search_turn(root, &provider, 4096, &mut trace).unwrap();
                let cue = if first.to_string().contains(central) {
                    central
                } else {
                    local
                };
                trace.batch += 1;
                let result = rt.search_turn(cue, &provider, 4096, &mut trace).unwrap();
                let id = expected[index];
                let visible = found(&result).filter_map(receipt_id).any(|seen| seen == id);
                if visible {
                    rt.learn_search(trigger, &trace, &format!("核验成功 [#{id}]"), &provider)
                        .unwrap();
                }
                println!("arm={arm} case={name} searches=2 receipt_visible={visible}");
                continue;
            }
            note(rt, &"无关日志填满历史窗口。".repeat(380));
            let before = judge.requests.lock().unwrap().len();
            rt.handle_user_message(&prompt);
            let requests = judge.requests.lock().unwrap();
            let calls = requests.len() - before;
            assert!(calls <= RELAY_STEPS);
            let receipt_visible = requests[before..]
                .iter()
                .flatten()
                .any(|result| result.contains(cases[index].1));
            println!(
                "arm={arm} case={name} calls={calls} receipt_visible={receipt_visible} active={:?}",
                active(rt, root).map(|(id, _)| id)
            );
        }
    }
    let inherited_errors = inherited_errors.lock().unwrap();
    assert!(inherited_errors.is_empty(), "{inherited_errors:?}");
    let identity_errors = identity_errors.lock().unwrap();
    assert!(
        identity_errors.iter().all(|event| event["message"]
            .as_str()
            .is_some_and(|text| text.contains("达到单回合最大步数上限 (3)"))),
        "{identity_errors:?}"
    );
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
            .search_turn(root, &provider, 4096, &mut SearchTurn::default())
            .unwrap();
        assert_eq!(
            found(&result).filter_map(receipt_id).any(|seen| seen == id),
            should_see
        );
    }
    println!("next_updates inherited=1 identity=0 heldout_recall inherited=true identity=false");
}
