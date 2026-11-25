use super::*;
use crate::config::{HostConfig, ProviderConfig};
use crate::hands::ToolSpec;
use crate::llm::{Brain, HttpBrain, ModelReply};
use crate::runtime::{state, HostCommand};
use std::sync::{mpsc, Arc, Mutex};

mod recursion;

const STATUS: &str = "进度状态归档：申请仅受理时标「待回执」，只有实际结果明确才标「已确认」。交接的特殊措辞另见一份标题为「交接凭证」的补充档案。";
const HANDOFF: &str = "交接凭证补录：对方若只回「我收到了，明早看」，状态词是「待接手确认」。对方明确回复「我同意接手」才标「交接已定」。";
const FILLER: &str = "这是一段无关记录，用于让先前的档案和经历滑出上下文。";

struct TracedBrain {
    live: bool,
    requests: Mutex<Vec<Vec<Value>>>,
}

impl Brain for TracedBrain {
    fn complete(
        &self,
        provider: &ProviderConfig,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<ModelReply, String> {
        let is_reflection = system == SYSTEM;
        if is_reflection {
            assert!(tools.is_empty());
        }
        self.requests.lock().unwrap().push(messages.to_vec());
        if self.live {
            return HttpBrain.complete(provider, model, system, messages, tools);
        }
        if !is_reflection {
            let text = "收到".to_owned();
            return Ok(ModelReply {
                message: json!({"role":"assistant","content":text}),
                text,
                calls: vec![],
            });
        }
        let visible = json!(messages).to_string();
        let (finding, next_cue) = if visible.contains(HANDOFF) {
            ("这笔交接应标待接手确认", None)
        } else if visible.contains(STATUS) {
            ("预约已受理应标待回执，交接另查补充档案", Some("交接凭证"))
        } else {
            ("这次报销只有受理证据", Some("进度状态"))
        };
        let text = format!(
            "REFLECTION={}",
            json!({"finding": finding, "next_cue": next_cue})
        );
        Ok(ModelReply {
            message: json!({"role":"assistant","content":text}),
            text,
            calls: vec![],
        })
    }

    fn test_connection(&self, _: &ProviderConfig, _: &str) -> Result<String, String> {
        unreachable!("the reflection trial does not request a connection test")
    }
}

fn note(rt: &HostRuntime, text: &str) -> Id {
    let id = rt.eja.notice(Source::Person, text).unwrap();
    state::complete_turn(&rt.eja, id, "").unwrap();
    id
}

fn seen_entries(request: &[Value]) -> Vec<Entry> {
    assert_eq!(request.len(), 1);
    let packet: Value = serde_json::from_str(request[0]["content"].as_str().unwrap()).unwrap();
    serde_json::from_value(packet["evidence"].clone()).unwrap()
}

fn dispatch(rt: &mut HostRuntime, feedback: Id, errors: &Mutex<Vec<String>>) -> (Id, Reflection) {
    let command: HostCommand =
        serde_json::from_value(json!({"cmd":"reflect","feedback":feedback})).unwrap();
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(command)).unwrap();
    drop(tx);
    rt.run(rx).unwrap();
    let errors = errors.lock().unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let entry = rt
        .eja
        .reflection_for(feedback)
        .unwrap()
        .expect("missing reflection");
    let Fact::Reflected(reflection) = entry.fact else {
        unreachable!()
    };
    println!(
        "reflection #{} {}",
        entry.id,
        serde_json::to_string(&reflection).unwrap()
    );
    (entry.id, reflection)
}

fn exercise(mut config: HostConfig, live: bool) {
    config.capabilities.clear();
    config.window_max_bytes = Some(2048);
    println!("model={}", config.resolve_judge().unwrap().1);
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("subject");
    let brain = Arc::new(TracedBrain {
        live,
        requests: Mutex::new(vec![]),
    });
    let errors = Arc::new(Mutex::new(Vec::new()));
    let open = || {
        let errors = errors.clone();
        HostRuntime::open_with_brain(
            data.clone(),
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
        .unwrap()
    };

    let mut rt = open();
    rt.save_config_request(config.clone().into(), None, false);
    let status = note(&rt, STATUS);
    let handoff = note(&rt, HANDOFF);
    let filler = FILLER.repeat(64);
    note(&rt, &filler);
    let feedback1 = note(&rt, "更正：报销只显示已受理，你却说办妥了。档案目录里这类约定的关键词是「进度状态」。请总结这次判断，并让下次复盘能查到那个旧档案。");
    let (id1, first) = dispatch(&mut rt, feedback1, &errors);
    assert_eq!(first.parent, None);
    assert!(first.shown.is_empty());
    assert!(first.next_cue.is_some(), "{first:?}");
    drop(rt);

    let mut rt = open();
    assert_eq!(
        rt.eja
            .observation_at(rt.eja.revision().unwrap())
            .unwrap()
            .unwrap()
            .id,
        id1
    );
    note(&rt, &filler);
    let feedback2 = note(&rt, "再更正：预约申请已受理，你写成了办妥。按旧约定核对状态词。如果旧档案还指向一个适用于交接的专用档案，请把那个标题作为下一轮线索。");
    let (id2, second) = dispatch(&mut rt, feedback2, &errors);
    assert_eq!(second.parent, Some(id1));
    assert!(second.shown.contains(&status), "{second:?}");
    assert!(!second.shown.contains(&feedback1), "{second:?}");
    assert!(!second.shown.contains(&handoff), "{second:?}");
    println!(
        "intermediate_exact_status={}",
        second.finding.contains("待回执")
    );
    assert!(second.next_cue.is_some(), "{second:?}");
    assert_eq!(
        rt.eja
            .observation_at(rt.eja.revision().unwrap())
            .unwrap()
            .unwrap()
            .id,
        id2
    );

    note(&rt, &filler);
    let feedback3 = note(&rt, "新案例更正：对方只回「我收到了，明早看」，我却把交接登记为已接手。请给出旧约定里的原状态词；当前线索适合时继续使用。");
    let (id3, held_out) = dispatch(&mut rt, feedback3, &errors);
    assert_eq!(held_out.parent, Some(id2));
    assert!(held_out.shown.contains(&handoff), "{held_out:?}");
    assert!(held_out.finding.contains("待接手确认"), "{held_out:?}");
    assert_eq!(dispatch(&mut rt, feedback3, &errors).0, id3);

    let requests = brain.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!json!(requests[0]).to_string().contains(STATUS));
    assert!(!json!(requests[0]).to_string().contains(HANDOFF));
    assert!(!json!(requests[1]).to_string().contains(HANDOFF));
    for (request, saved) in requests.iter().zip([first, second, held_out]) {
        let actual: Vec<_> = seen_entries(request)
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(actual, saved.shown);
    }
}

#[test]
fn successive_reflections_use_prior_cues_and_current_feedback() {
    let config = serde_json::from_value(json!({
        "providers":[{"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:9999/v1"}],
        "judge":{"provider_id":"fixture","model":"fixture"}
    }))
    .unwrap();
    exercise(config, false);
}

#[test]
fn reflection_keeps_latest_receipt_and_discards_unseen_cue() {
    let dir = tempfile::tempdir().unwrap();
    let brain = Arc::new(TracedBrain {
        live: false,
        requests: Mutex::new(Vec::new()),
    });
    let mut rt =
        HostRuntime::open_with_brain(dir.path().join("subject"), brain.clone(), Arc::new(|_| {}))
            .unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "providers":[{"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:9999/v1"}],
        "judge":{"provider_id":"fixture","model":"fixture"},
        "window_max_bytes":2048,
    }))
    .unwrap();
    rt.save_config_request(config.into(), None, false);
    let action = rt
        .eja
        .act(rt.eja.revision().unwrap(), "inspect", Value::Null)
        .unwrap();
    let stale = rt
        .eja
        .finish(action.id, Outcome::Unknown("水闸状态未知".into()))
        .unwrap();
    let confirmed = rt
        .eja
        .finish(action.id, Outcome::Observed("已经实测关闭".into()))
        .unwrap();
    let first = note(&rt, "下次查水闸");
    let active = rt
        .eja
        .record_reflection(Reflection {
            parent: None,
            through: first,
            feedback: first,
            shown: vec![],
            finding: "下次查水闸".into(),
            when: None,
            next_cue: Some("水闸".into()),
            retire: false,
        })
        .unwrap();
    let second = note(
        &rt,
        &format!("核对这次实测回执。{}", "与判断无关。".repeat(200)),
    );
    let result = rt.reflect(second).unwrap();
    let Fact::Reflected(record) = result.fact else {
        unreachable!()
    };
    assert_eq!(record.parent, Some(active.id));
    assert!(record.shown.contains(&confirmed.id));
    assert!(!record.shown.contains(&stale.id));
    assert_eq!(record.next_cue, None); // the scripted judge emits "进度状态", which was not shown
    let requests = brain.requests.lock().unwrap();
    let sent = seen_entries(&requests[0]);
    assert_eq!(
        sent.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        record.shown
    );
    assert!(msg_bytes(&requests[0][0]) <= 2048);
}

#[test]
fn turn_recall_transfers_multi_topic_reflections_without_duplication_or_noise() {
    let dir = tempfile::tempdir().unwrap();
    let brain = Arc::new(TracedBrain {
        live: false,
        requests: Mutex::new(Vec::new()),
    });
    let mut rt =
        HostRuntime::open_with_brain(dir.path().join("subject"), brain.clone(), Arc::new(|_| {}))
            .unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "providers":[{"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:9999/v1"}],
        "judge":{"provider_id":"fixture","model":"fixture"},
        "window_max_bytes":2048,
    }))
    .unwrap();
    rt.save_config_request(config.into(), None, false);

    let action = rt
        .eja
        .act(rt.eja.revision().unwrap(), "inspect", Value::Null)
        .unwrap();
    let _stale = rt
        .eja
        .finish(action.id, Outcome::Unknown("ARC-ALPHA 状态未知".into()))
        .unwrap();
    let confirmed_a = rt
        .eja
        .finish(
            action.id,
            Outcome::Observed("ARC-ALPHA 场地方确认函凭证 P-ALPHA".into()),
        )
        .unwrap();
    let receipt_b = note(&rt, "ARC-BETA 仓库签章回执凭证 P-BETA");

    let f_a = note(&rt, "纠正：场地预约查 ARC-ALPHA");
    rt.eja
        .record_reflection(Reflection {
            parent: None,
            through: f_a,
            feedback: f_a,
            shown: vec![],
            finding: "场地预约线索".into(),
            when: Some("场地预约".into()),
            next_cue: Some("ARC-ALPHA".into()),
            retire: false,
        })
        .unwrap();
    let f_b = note(&rt, "纠正：物料验收查 ARC-BETA");
    rt.eja
        .record_reflection(Reflection {
            parent: None,
            through: f_b,
            feedback: f_b,
            shown: vec![],
            finding: "物料验收线索".into(),
            when: Some("物料验收".into()),
            next_cue: Some("ARC-BETA".into()),
            retire: false,
        })
        .unwrap();

    // While facts are still inside the window (anchor == 0), turn recall injects nothing.
    rt.handle_user_message("核对场地预约状态");
    {
        let requests = brain.requests.lock().unwrap();
        let payload = json!(requests[0]).to_string();
        assert!(!payload.contains("reflection_recall"), "{payload}");
    }

    // Push earlier records out of the window.
    note(&rt, &FILLER.repeat(64));

    // Topic A (earlier than Topic B) must still recall its latest receipt, not Topic B's.
    rt.handle_user_message("请核对场地预约凭证");
    // Topic B recalls Topic B's receipt.
    rt.handle_user_message("请核对物料验收凭证");
    // Unrelated topic injects zero reflection recall.
    rt.handle_user_message("明天天气怎么样");

    let requests = brain.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);

    let req_a = json!(requests[1]).to_string();
    assert!(req_a.contains("reflection_recall"), "{req_a}");
    assert!(req_a.contains("P-ALPHA"), "{req_a}");
    assert!(!req_a.contains("状态未知"), "{req_a}");
    assert!(!req_a.contains("P-BETA"), "{req_a}");
    let recall_a: Value = requests[1]
        .iter()
        .filter_map(|msg| msg["content"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|packet| packet.get("reflection_recall").is_some())
        .expect("missing reflection_recall packet for Topic A");
    let ids_a: Vec<Id> = serde_json::from_value::<Vec<Entry>>(recall_a["evidence"].clone())
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert!(ids_a.contains(&confirmed_a.id), "{ids_a:?}");
    assert!(crate::llm::messages_bytes(&requests[1]) <= 2048);

    let req_b = json!(requests[2]).to_string();
    assert!(req_b.contains("reflection_recall"), "{req_b}");
    assert!(req_b.contains("P-BETA"), "{req_b}");
    assert!(!req_b.contains("P-ALPHA"), "{req_b}");
    let recall_b: Value = requests[2]
        .iter()
        .filter_map(|msg| msg["content"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|packet| packet.get("reflection_recall").is_some())
        .expect("missing reflection_recall packet for Topic B");
    let ids_b: Vec<Id> = serde_json::from_value::<Vec<Entry>>(recall_b["evidence"].clone())
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert!(ids_b.contains(&receipt_b), "{ids_b:?}");
    assert!(crate::llm::messages_bytes(&requests[2]) <= 2048);

    let req_unrelated = json!(requests[3]).to_string();
    assert!(
        !req_unrelated.contains("reflection_recall"),
        "{req_unrelated}"
    );
    assert!(!req_unrelated.contains("P-ALPHA"), "{req_unrelated}");
    assert!(!req_unrelated.contains("P-BETA"), "{req_unrelated}");
}

#[test]
fn turn_reflect_tool_grounds_evolves_pairs_actions_and_retires_with_semantic_matching() {
    struct MockEncoder;
    impl eja_memory::Encoder for MockEncoder {
        fn identity(&self) -> &str {
            "mock-semantic-v1"
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn encode(
            &mut self,
            _kind: eja_memory::InputKind,
            texts: &[&str],
        ) -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(texts
                .iter()
                .map(|text| {
                    if text.contains("场地预约") || text.contains("会议室") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let brain = Arc::new(TracedBrain {
        live: false,
        requests: Mutex::new(Vec::new()),
    });
    let mut rt =
        HostRuntime::open_with_brain(dir.path().join("subject"), brain.clone(), Arc::new(|_| {}))
            .unwrap();
    rt.memory = eja_memory::MemoryIndex::open(dir.path().join("memory.db"), MockEncoder).unwrap();
    let config: HostConfig = serde_json::from_value(json!({
        "providers":[{"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:9999/v1"}],
        "judge":{"provider_id":"fixture","model":"fixture"},
        "window_max_bytes":2048,
    }))
    .unwrap();
    rt.save_config_request(config.into(), None, false);

    let action = rt
        .eja
        .act(
            rt.eja.revision().unwrap(),
            "shell",
            json!({"command": "verify_venue --code ARC-VENUE"}),
        )
        .unwrap();
    let receipt = rt
        .eja
        .finish(
            action.id,
            Outcome::Observed("ARC-VENUE 已签发凭证 P-VENUE".into()),
        )
        .unwrap();

    let turn1 = note(&rt, "更正：场地预约以后先查 ARC-VENUE");
    let mut ctx1 = TurnReflectionContext::new(turn1, 0);
    ctx1.note_seen(receipt.id);

    let bad = rt
        .apply_turn_reflection(
            &json!({"when": "场地预约", "next_cue": "HALLUCINATED", "finding": "测试"}),
            &ctx1,
        )
        .unwrap();
    assert!(bad.get("error").is_some(), "{bad}");

    let created = rt
        .apply_turn_reflection(
            &json!({"when": "场地预约", "next_cue": "ARC-VENUE", "finding": "绑定场地预约检索线索"}),
            &ctx1,
        )
        .unwrap();
    assert_eq!(created["status"], "reflected", "{created}");
    let rule_id = created["entry"]["id"].as_i64().unwrap();

    note(&rt, &FILLER.repeat(64));
    rt.handle_user_message("明天的会议室订好了吗？");
    {
        let requests = brain.requests.lock().unwrap();
        let payload = json!(requests[0]).to_string();
        assert!(payload.contains("observations"), "{payload}");
        assert!(payload.contains("reflection_recall"), "{payload}");
        assert!(payload.contains("verify_venue"), "{payload}");
        assert!(payload.contains("P-VENUE"), "{payload}");
    }

    let turn_retire = note(&rt, "场地预约线索已作废，请退役");
    let ctx_retire = TurnReflectionContext::new(turn_retire, rt.anchor);
    let retired = rt
        .apply_turn_reflection(
            &json!({"retire": true, "rule": rule_id, "finding": "场地预约线索已作废"}),
            &ctx_retire,
        )
        .unwrap();
    assert_eq!(retired["status"], "retired", "{retired}");
    assert!(rt
        .eja
        .observations_at(rt.eja.revision().unwrap())
        .unwrap()
        .is_empty());

    note(&rt, &FILLER.repeat(64));
    rt.handle_user_message("明天的会议室订好了吗？");
    let requests = brain.requests.lock().unwrap();
    let after = json!(requests[1]).to_string();
    assert!(!after.contains("observations"), "{after}");
    assert!(!after.contains("reflection_recall"), "{after}");
}

#[test]
#[ignore = "requires a live judge from EJA_EVAL_CONFIG; uses synthetic history"]
fn reflection_live_two_generations() {
    let path =
        std::env::var_os("EJA_EVAL_CONFIG").expect("set EJA_EVAL_CONFIG to a model config.json");
    let config = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    exercise(config, true);
}
