use super::{dispatch, note, Arc, HostConfig, HostRuntime, Mutex, TracedBrain, FILLER};
use eja_core::{Eja, Id, Timestamp};
use serde_json::json;
use sha2::{Digest, Sha256};

struct Scenario {
    topic: &'static str,
    authority: &'static str,
    decoy: &'static str,
    receipt_exists: bool,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        topic: "场地预约",
        authority: "场地方确认函",
        decoy: "内部预留单",
        receipt_exists: true,
    },
    Scenario {
        topic: "物料验收",
        authority: "仓库签章回执",
        decoy: "采购申请草稿",
        receipt_exists: true,
    },
    Scenario {
        topic: "值班交接",
        authority: "接班人签收",
        decoy: "交班人预填记录",
        receipt_exists: true,
    },
    Scenario {
        topic: "报销结算",
        authority: "银行入账凭证",
        decoy: "付款申请单",
        receipt_exists: true,
    },
    Scenario {
        topic: "设备归还",
        authority: "库房签收回执",
        decoy: "领用人归还申请",
        receipt_exists: false,
    },
    Scenario {
        topic: "场刊定稿",
        authority: "主编签发单",
        decoy: "编辑部草稿",
        receipt_exists: false,
    },
];

fn opaque_id(seed: u64, index: usize, field: &str) -> String {
    let hash = format!("{:x}", Sha256::digest(format!("{seed}:{index}:{field}")));
    hash[..12].to_ascii_uppercase()
}

struct Trial {
    recursive: HostRuntime,
    frozen: HostRuntime,
    recursive_brain: Arc<TracedBrain>,
    frozen_brain: Arc<TracedBrain>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Trial {
    fn open(root: &std::path::Path, config: &HostConfig) -> Self {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let open = |name: &str| {
            let brain = Arc::new(TracedBrain {
                live: true,
                requests: Mutex::new(vec![]),
            });
            let errors = errors.clone();
            let mut rt = HostRuntime::open_with_brain(
                root.join(name),
                brain.clone(),
                Arc::new(move |event| {
                    if event["event"] == "error" {
                        errors
                            .lock()
                            .unwrap()
                            .push(event["message"].as_str().unwrap().to_owned());
                    }
                }),
            )
            .unwrap();
            let path = rt.core_dir.path().join("subject.db");
            rt.eja = Eja::open(path)
                .unwrap()
                .with_clock(|| Timestamp(1_700_000_000));
            rt.save_config_request(config.clone().into(), None, false);
            (rt, brain)
        };
        let (recursive, recursive_brain) = open("recursive");
        let (frozen, frozen_brain) = open("frozen");
        Self {
            recursive,
            frozen,
            recursive_brain,
            frozen_brain,
            errors,
        }
    }

    fn note_both(&self, text: &str) -> Id {
        let id = note(&self.recursive, text);
        assert_eq!(id, note(&self.frozen, text));
        id
    }
}

#[derive(Default)]
struct Totals {
    inherited: usize,
    available: usize,
    recursive_receipt_visible: usize,
    recursive_code_mentioned: usize,
    recursive_correct: usize,
    frozen_correct: usize,
    absent: usize,
    recursive_abstained: usize,
    frozen_abstained: usize,
    frozen_unknown_when_available: usize,
}

fn single_word(text: &str) -> &str {
    text.trim()
        .trim_matches(|c| matches!(c, '「' | '」' | '"' | '。' | '`'))
}

fn exercise(
    config: &HostConfig,
    seed: u64,
    index: usize,
    scenario: &Scenario,
    totals: &mut Totals,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut trial = Trial::open(dir.path(), config);
    let entrance = format!("IDX-{}", opaque_id(seed, index, "index"));
    let authority_key = format!("ARC-{}", opaque_id(seed, index, "authority"));
    let decoy_key = format!("ARC-{}", opaque_id(seed, index, "decoy"));
    let code = format!("P-{}", opaque_id(seed, index, "answer"));
    let decoy = format!("P-{}", opaque_id(seed, index, "decoy-answer"));
    let good_line = format!("{}的检索标签是「{authority_key}」。", scenario.authority);
    let bad_line = format!("{}的检索标签是「{decoy_key}」。", scenario.decoy);
    let mut lines = [good_line, bad_line];
    if !index.is_multiple_of(2) {
        lines.swap(0, 1);
    }
    let directory = trial.note_both(&format!(
        "档案目录 {entrance}，记录事项「{}」。{}",
        scenario.topic,
        lines.concat()
    ));
    let receipt = scenario.receipt_exists.then(|| {
        trial.note_both(&format!(
            "档案 {authority_key}：{}已签发，原始凭证码为「{code}」。",
            scenario.authority
        ))
    });
    trial.note_both(&format!(
        "档案 {decoy_key}：{}中的预填凭证码为「{decoy}」。",
        scenario.decoy
    ));
    let filler = FILLER.repeat(64);

    trial.note_both(&filler);
    let first_feedback = trial.note_both(&format!(
        "纠正：{}应先查登记的档案目录，目录只有一个标签「{entrance}」。请给下一次反思使用的线索，next_cue 原样写这个标签。",
        scenario.topic
    ));
    let (first_id, first) = dispatch(&mut trial.recursive, first_feedback, &trial.errors);
    assert_eq!(
        trial.frozen.eja.record_reflection(first).unwrap().id,
        first_id
    );

    trial.note_both(&filler);
    let second_feedback = trial.note_both(&format!(
        "纠正：核对{}的正式凭证时，以{}为准。请根据这次实际看到的目录，决定下一次该查哪个档案。next_cue 只写一个最具体的原样标签。没看到目录就填 null。",
        scenario.topic, scenario.authority
    ));
    let (second_id, second) = dispatch(&mut trial.recursive, second_feedback, &trial.errors);
    let mut frozen_second = second.clone();
    frozen_second.next_cue = None;
    frozen_second.when = None;
    assert_eq!(
        trial
            .frozen
            .eja
            .record_reflection(frozen_second)
            .unwrap()
            .id,
        second_id
    );

    trial.note_both(&filler);
    let final_feedback = trial.note_both(&format!(
        "新案例：请从{}给出{}的正式凭证码。finding 只写一个原样的凭证码。没看到该凭证就只写「未知」。next_cue 填 null。",
        scenario.authority, scenario.topic
    ));
    let (recursive, frozen) = if (index / 2).is_multiple_of(2) {
        let (_, a) = dispatch(&mut trial.recursive, final_feedback, &trial.errors);
        let (_, b) = dispatch(&mut trial.frozen, final_feedback, &trial.errors);
        (a, b)
    } else {
        let (_, b) = dispatch(&mut trial.frozen, final_feedback, &trial.errors);
        let (_, a) = dispatch(&mut trial.recursive, final_feedback, &trial.errors);
        (a, b)
    };
    let actual = trial.recursive_brain.requests.lock().unwrap();
    let control = trial.frozen_brain.requests.lock().unwrap();
    assert_eq!(actual.len(), 3);
    assert_eq!(control.len(), 1);
    assert!(!json!(actual[0]).to_string().contains(&authority_key));
    assert!(!json!(actual[1]).to_string().contains(&code));
    assert!(!json!(control[0]).to_string().contains(&code));
    let code_visible = json!(actual[2]).to_string().contains(&code);
    assert_eq!(
        code_visible,
        receipt.is_some_and(|id| recursive.shown.contains(&id)),
        "the code must be visible through the corresponding archival fact"
    );
    for (request, result) in [(&actual[2], &recursive), (&control[0], &frozen)] {
        let visible_ids: Vec<_> = super::seen_entries(request)
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(visible_ids, result.shown);
    }
    let inherited = second.parent == Some(first_id)
        && second.shown.contains(&directory)
        && second.next_cue.as_deref() == Some(authority_key.as_str())
        && recursive.parent == Some(second_id)
        && frozen.parent == Some(first_id);
    totals.inherited += usize::from(inherited);
    if let Some(receipt) = receipt {
        totals.available += 1;
        totals.recursive_receipt_visible += usize::from(code_visible);
        totals.recursive_code_mentioned += usize::from(recursive.finding.contains(&code));
        totals.recursive_correct += usize::from(
            inherited
                && recursive.shown.contains(&receipt)
                && single_word(&recursive.finding) == code,
        );
        totals.frozen_correct += usize::from(single_word(&frozen.finding) == code);
        totals.frozen_unknown_when_available += usize::from(single_word(&frozen.finding) == "未知");
    } else {
        totals.absent += 1;
        totals.recursive_abstained += usize::from(single_word(&recursive.finding) == "未知");
        totals.frozen_abstained += usize::from(single_word(&frozen.finding) == "未知");
    }
    println!("case={} receipt={receipt:?} inherited={inherited} code_visible={code_visible} expected={} decoy={} recursive={} frozen={}",
        scenario.topic, if receipt.is_some() { &code } else { "未知" }, decoy,
        recursive.finding, frozen.finding);
}

#[test]
#[ignore = "requires a live judge; compares active and frozen observation states on synthetic histories"]
fn reflection_live_causal_inheritance() {
    let path =
        std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG to a model config.json");
    let mut config: HostConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    config.capabilities.clear();
    config.window_max_bytes = Some(2048);
    let seed = std::env::var("EJA_RSL_TEST_SEED")
        .map_or(Ok(20261125), |value| value.parse())
        .unwrap();
    println!("model={} seed={seed}", config.resolve_judge().unwrap().1);
    let mut totals = Totals::default();
    for (index, scenario) in SCENARIOS.iter().enumerate() {
        exercise(&config, seed, index, scenario, &mut totals);
    }
    println!(
        "inheritance={}/{} receipt_visible={}/{} code_mentioned={}/{} grounded={}/{} frozen_correct={}/{} frozen_unknown_with_receipt={}/{} absent_abstentions_active={}/{} absent_abstentions_frozen={}/{}",
        totals.inherited, SCENARIOS.len(), totals.recursive_receipt_visible, totals.available,
        totals.recursive_code_mentioned, totals.available, totals.recursive_correct, totals.available,
        totals.frozen_correct, totals.available, totals.frozen_unknown_when_available, totals.available,
        totals.recursive_abstained, totals.absent, totals.frozen_abstained, totals.absent,
    );
    assert!(
        totals.inherited >= 5,
        "insufficient inherited observation transitions"
    );
    assert!(
        totals.recursive_correct >= 3,
        "insufficient grounded hold-out answers"
    );
    assert_eq!(totals.frozen_correct, 0);
    assert_eq!(totals.frozen_unknown_when_available, totals.available);
    assert_eq!(totals.recursive_abstained, totals.absent);
    assert_eq!(totals.frozen_abstained, totals.absent);
}

fn latest_said(rt: &HostRuntime) -> String {
    let head = rt.eja.revision().unwrap();
    let entries = rt.eja.entries_after(0, head).unwrap();
    entries
        .into_iter()
        .rev()
        .find_map(|entry| match entry.fact {
            eja_core::Fact::Said { text } => Some(text),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "requires a live judge; proves interleaved multi-topic recursion transfers to live send turns without forgetting or unrelated noise"]
fn reflection_live_multi_topic_turn_transfer() {
    let path =
        std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG to a model config.json");
    let mut config: HostConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    config.capabilities.clear();
    config.window_max_bytes = Some(2048);
    let seed = std::env::var("EJA_RSL_TEST_SEED")
        .map_or(Ok(20261125), |value| value.parse())
        .unwrap();
    println!("model={} seed={seed}", config.resolve_judge().unwrap().1);

    let dir = tempfile::tempdir().unwrap();
    let mut trial = Trial::open(dir.path(), &config);
    let topics = [&SCENARIOS[0], &SCENARIOS[1]];
    let mut keys = Vec::new();
    for (index, scenario) in topics.iter().enumerate() {
        let entrance = format!("IDX-{}", opaque_id(seed, 100 + index, "index"));
        let authority_key = format!("ARC-{}", opaque_id(seed, 100 + index, "authority"));
        let decoy_key = format!("ARC-{}", opaque_id(seed, 100 + index, "decoy"));
        let code = format!("P-{}", opaque_id(seed, 100 + index, "answer"));
        let decoy = format!("P-{}", opaque_id(seed, 100 + index, "decoy-answer"));
        let directory = trial.note_both(&format!(
            "档案目录 {entrance}，记录事项「{}」。{}的检索标签是「{authority_key}」。{}的检索标签是「{decoy_key}」。",
            scenario.topic, scenario.authority, scenario.decoy
        ));
        let receipt = trial.note_both(&format!(
            "档案 {authority_key}：{}已签发，原始凭证码为「{code}」。",
            scenario.authority
        ));
        trial.note_both(&format!(
            "档案 {decoy_key}：{}中的预填凭证码为「{decoy}」。",
            scenario.decoy
        ));
        keys.push((entrance, authority_key, code, directory, receipt));
    }
    let filler = FILLER.repeat(64);

    // Interleave generation 1 across Topic A and Topic B.
    let mut gen1_ids = Vec::new();
    for (index, scenario) in topics.iter().enumerate() {
        trial.note_both(&filler);
        let feedback = trial.note_both(&format!(
            "纠正：{}应先查登记的档案目录，目录只有一个标签「{}」。when 写「{}」，next_cue 原样写这个标签。",
            scenario.topic, keys[index].0, scenario.topic
        ));
        let (id, first) = dispatch(&mut trial.recursive, feedback, &trial.errors);
        assert_eq!(first.when.as_deref(), Some(scenario.topic), "{first:?}");
        assert_eq!(
            first.next_cue.as_deref(),
            Some(keys[index].0.as_str()),
            "{first:?}"
        );
        assert_eq!(trial.frozen.eja.record_reflection(first).unwrap().id, id);
        gen1_ids.push(id);
    }

    // Interleave generation 2 across Topic A then Topic B (Topic B runs AFTER Topic A!).
    for (index, scenario) in topics.iter().enumerate() {
        trial.note_both(&filler);
        let feedback = trial.note_both(&format!(
            "纠正：核对{}的正式凭证时，以{}为准。请根据这次实际看到的目录，更新{}的检索线索。when 保持「{}」，next_cue 只写最具体的原样标签。",
            scenario.topic, scenario.authority, scenario.topic, scenario.topic
        ));
        let (id, second) = dispatch(&mut trial.recursive, feedback, &trial.errors);
        assert_eq!(second.parent, Some(gen1_ids[index]), "{second:?}");
        assert!(second.shown.contains(&keys[index].3), "{second:?}");
        assert_eq!(second.when.as_deref(), Some(scenario.topic), "{second:?}");
        assert_eq!(
            second.next_cue.as_deref(),
            Some(keys[index].1.as_str()),
            "{second:?}"
        );
        let mut frozen_second = second.clone();
        frozen_second.next_cue = None;
        frozen_second.when = None;
        assert_eq!(
            trial
                .frozen
                .eja
                .record_reflection(frozen_second)
                .unwrap()
                .id,
            id
        );
    }

    // Push all prior facts out of the live window, then test real `handle_user_message` (`send`) turns!
    trial.note_both(&filler);
    let mut recursive_step1_hits = 0usize;
    let mut frozen_step1_hits = 0usize;
    let mut recursive_direct_correct = 0usize;
    for (index, scenario) in topics.iter().enumerate() {
        let prompt = format!(
            "请根据当前上下文中已有的原始资料直接给出{}中{}的凭证码。不要调用工具；如果当前上下文里没有看到该凭证码，就只回复「未知」。",
            scenario.topic, scenario.authority
        );
        let rec_before = trial.recursive_brain.requests.lock().unwrap().len();
        let frz_before = trial.frozen_brain.requests.lock().unwrap().len();
        trial.recursive.handle_user_message(&prompt);
        trial.frozen.handle_user_message(&prompt);
        assert!(trial.errors.lock().unwrap().is_empty());

        let rec_reqs = trial.recursive_brain.requests.lock().unwrap();
        let frz_reqs = trial.frozen_brain.requests.lock().unwrap();
        let rec_step1 = json!(rec_reqs[rec_before]).to_string();
        let frz_step1 = json!(frz_reqs[frz_before]).to_string();
        let rec_has_code = rec_step1.contains(&keys[index].2);
        let frz_has_code = frz_step1.contains(&keys[index].2);
        let rec_reply = latest_said(&trial.recursive);
        let frz_reply = latest_said(&trial.frozen);
        recursive_step1_hits += usize::from(rec_has_code);
        frozen_step1_hits += usize::from(frz_has_code);
        recursive_direct_correct +=
            usize::from(rec_reqs.len() == rec_before + 1 && rec_reply.contains(&keys[index].2));
        println!(
            "send topic={} expected={} rec_step1_code={rec_has_code} frz_step1_code={frz_has_code} rec_steps={} rec_reply={rec_reply:?} frz_reply={frz_reply:?}",
            scenario.topic,
            keys[index].2,
            rec_reqs.len() - rec_before
        );
    }

    // Unrelated live turn must NOT receive any reflection recall injection.
    let unrelated_before = trial.recursive_brain.requests.lock().unwrap().len();
    trial
        .recursive
        .handle_user_message("请直接回答：2加3等于几？只写一个阿拉伯数字。");
    let rec_reqs = trial.recursive_brain.requests.lock().unwrap();
    let unrelated_req = json!(rec_reqs[unrelated_before]).to_string();
    let unrelated_clean = !unrelated_req.contains("reflection_recall");
    let unrelated_reply = latest_said(&trial.recursive);
    println!("send topic=unrelated clean={unrelated_clean} reply={unrelated_reply:?}");

    assert_eq!(recursive_step1_hits, 2);
    assert_eq!(frozen_step1_hits, 0);
    assert_eq!(recursive_direct_correct, 2);
    assert!(unrelated_clean);
}

#[test]
#[ignore = "requires a live judge; proves full autonomous RSI via send-only turns (create, gen-2 evolve, 1-step transfer, zero-overlap paraphrase, retire)"]
fn reflection_live_autonomous_full_rsi() {
    let path =
        std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG to a model config.json");
    let mut config: HostConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    config.capabilities.clear();
    config.window_max_bytes = Some(2048);
    let seed = std::env::var("EJA_RSL_TEST_SEED")
        .map_or(Ok(2026112507), |value| value.parse())
        .unwrap();
    println!("model={} seed={seed}", config.resolve_judge().unwrap().1);

    let dir = tempfile::tempdir().unwrap();
    let mut trial = Trial::open(dir.path(), &config);
    let scenario = &SCENARIOS[0]; // 场地预约 / 场地方确认函 / 内部预留单
    let entrance = format!("IDX-{}", opaque_id(seed, 200, "index"));
    let authority_key = format!("ARC-{}", opaque_id(seed, 200, "authority"));
    let decoy_key = format!("ARC-{}", opaque_id(seed, 200, "decoy"));
    let code = format!("P-{}", opaque_id(seed, 200, "answer"));
    let decoy = format!("P-{}", opaque_id(seed, 200, "decoy-answer"));
    trial.note_both(&format!(
        "档案目录 {entrance}，记录事项「{}」。{}的检索标签是「{authority_key}」。{}的检索标签是「{decoy_key}」。",
        scenario.topic, scenario.authority, scenario.decoy
    ));
    trial.note_both(&format!(
        "档案 {authority_key}：{}已签发，原始凭证码为「{code}」。",
        scenario.authority
    ));
    trial.note_both(&format!(
        "档案 {decoy_key}：{}中的预填凭证码为「{decoy}」。",
        scenario.decoy
    ));
    let filler = FILLER.repeat(64);

    // Phase 1: Autonomous Gen-1 learning via plain `send` (zero external `reflect` commands).
    trial.note_both(&filler);
    trial.recursive.handle_user_message(&format!(
        "纠正：{}以后查正式凭证前要先查登记的档案目录，目录标签是「{entrance}」。请用 reflect 工具记录这条检索线索（when 填「{}」，next_cue 填「{entrance}」），再简短确认。",
        scenario.topic, scenario.topic
    ));
    assert!(
        trial.errors.lock().unwrap().is_empty(),
        "{:?}",
        trial.errors.lock().unwrap()
    );
    let gen1_obs = trial
        .recursive
        .eja
        .observations_at(trial.recursive.eja.revision().unwrap())
        .unwrap();
    assert_eq!(gen1_obs.len(), 1, "{gen1_obs:?}");
    let gen1_id = gen1_obs[0].id;
    let eja_core::Fact::Reflected(gen1_ref) = &gen1_obs[0].fact else {
        unreachable!()
    };
    assert_eq!(gen1_ref.when.as_deref(), Some(scenario.topic));
    assert_eq!(gen1_ref.next_cue.as_deref(), Some(entrance.as_str()));
    println!("phase1_gen1 id={gen1_id} ref={gen1_ref:?}");

    // Mirror Gen-1 into frozen control (control stays frozen at Gen-1).
    let f_ctrl = note(&trial.frozen, "冻结基线：记录第一代线索");
    trial
        .frozen
        .eja
        .record_reflection(eja_core::Reflection {
            parent: None,
            through: f_ctrl,
            feedback: f_ctrl,
            shown: vec![],
            finding: "冻结在第一代线索".into(),
            when: Some(scenario.topic.into()),
            next_cue: Some(entrance.clone()),
            retire: false,
        })
        .unwrap();

    // Phase 2: Autonomous Gen-2 recursive evolution via plain `send`.
    note(&trial.recursive, &filler);
    note(&trial.frozen, &filler);
    trial.recursive.handle_user_message(&format!(
        "核对{}的正式凭证时以{}为准。请根据自动召回的目录定位{}的检索标签，调用 search 查出凭证码，并用 reflect 将「{}」的 next_cue 升级为那个最终检索标签，最后回复凭证码。",
        scenario.topic, scenario.authority, scenario.authority, scenario.topic
    ));
    assert!(
        trial.errors.lock().unwrap().is_empty(),
        "{:?}",
        trial.errors.lock().unwrap()
    );
    let gen2_obs = trial
        .recursive
        .eja
        .observations_at(trial.recursive.eja.revision().unwrap())
        .unwrap();
    assert_eq!(gen2_obs.len(), 1, "{gen2_obs:?}");
    let gen2_id = gen2_obs[0].id;
    let eja_core::Fact::Reflected(gen2_ref) = &gen2_obs[0].fact else {
        unreachable!()
    };
    assert_eq!(gen2_ref.parent, Some(gen1_id), "{gen2_ref:?}");
    assert_eq!(gen2_ref.when.as_deref(), Some(scenario.topic));
    assert_eq!(gen2_ref.next_cue.as_deref(), Some(authority_key.as_str()));
    let phase2_reply = latest_said(&trial.recursive);
    println!("phase2_gen2 id={gen2_id} ref={gen2_ref:?} reply={phase2_reply:?}");

    // Phase 3a: Direct 1-step transfer vs frozen control.
    note(&trial.recursive, &filler);
    note(&trial.frozen, &filler);
    let direct_prompt = format!(
        "请根据当前上下文中已有的原始资料直接给出{}中{}的凭证码。不要调用工具；如果当前上下文里没有看到该凭证码，就只回复「未知」。",
        scenario.topic, scenario.authority
    );
    let rec_before = trial.recursive_brain.requests.lock().unwrap().len();
    let frz_before = trial.frozen_brain.requests.lock().unwrap().len();
    trial.recursive.handle_user_message(&direct_prompt);
    trial.frozen.handle_user_message(&direct_prompt);
    let rec_step1 = json!(trial.recursive_brain.requests.lock().unwrap()[rec_before]).to_string();
    let frz_step1 = json!(trial.frozen_brain.requests.lock().unwrap()[frz_before]).to_string();
    let rec_steps = trial.recursive_brain.requests.lock().unwrap().len() - rec_before;
    let rec_direct = latest_said(&trial.recursive);
    let frz_direct = latest_said(&trial.frozen);
    println!(
        "phase3a_direct expected={code} rec_step1={} frz_step1={} rec_steps={rec_steps} rec={rec_direct:?} frz={frz_direct:?}",
        rec_step1.contains(&code),
        frz_step1.contains(&code),
    );
    assert!(rec_step1.contains(&code));
    assert!(!frz_step1.contains(&code));
    assert_eq!(rec_steps, 1);
    assert!(rec_direct.contains(&code), "{rec_direct}");
    assert!(frz_direct.contains("未知"), "{frz_direct}");

    // Phase 3b: Zero-character-overlap paraphrase resolved via anchor_header observations.
    note(&trial.recursive, &filler);
    let paraphrase = "明天的会议室订好了吗？请查出对方签发的确认函凭证码并直接告诉我。";
    for ch in scenario.topic.chars() {
        assert!(
            !paraphrase.contains(ch),
            "paraphrase must share zero characters with scope"
        );
    }
    let para_before = trial.recursive_brain.requests.lock().unwrap().len();
    trial.recursive.handle_user_message(paraphrase);
    let para_step1 = json!(trial.recursive_brain.requests.lock().unwrap()[para_before]).to_string();
    let para_reply = latest_said(&trial.recursive);
    println!(
        "phase3b_paraphrase has_obs_index={} reply={para_reply:?}",
        para_step1.contains(&authority_key),
    );
    assert!(para_step1.contains("observations"), "{para_step1}");
    assert!(para_step1.contains(&authority_key), "{para_step1}");
    assert!(para_reply.contains(&code), "{para_reply}");

    // Phase 4: Autonomous retirement via plain `send`, then confirm zero recall afterwards.
    note(&trial.recursive, &filler);
    trial.recursive.handle_user_message(&format!(
        "通知：「{}」的旧检索线索已经作废，请立即调用 reflect 工具将它退役（retire 设为 true），以后不要再保留或自动召回。",
        scenario.topic
    ));
    assert!(
        trial.errors.lock().unwrap().is_empty(),
        "{:?}",
        trial.errors.lock().unwrap()
    );
    let after_retire = trial
        .recursive
        .eja
        .observations_at(trial.recursive.eja.revision().unwrap())
        .unwrap();
    println!("phase4_after_retire active_count={}", after_retire.len());
    assert!(after_retire.is_empty(), "{after_retire:?}");

    note(&trial.recursive, &filler);
    let verify_before = trial.recursive_brain.requests.lock().unwrap().len();
    trial.recursive.handle_user_message(&direct_prompt);
    let verify_step1 =
        json!(trial.recursive_brain.requests.lock().unwrap()[verify_before]).to_string();
    let verify_reply = latest_said(&trial.recursive);
    println!(
        "phase4_verify_clean has_recall={} has_obs={} reply={verify_reply:?}",
        verify_step1.contains("reflection_recall"),
        verify_step1.contains(&authority_key),
    );
    assert!(!verify_step1.contains("reflection_recall"));
    assert!(!verify_step1.contains(&authority_key));
    assert!(verify_reply.contains("未知"), "{verify_reply}");
}
