use eja::adapters::{render_context, OpenAiBrain};
use eja::{
    project_with_evidence, Action, Brain, EventMeta, Kind, LookSource, Observation, ProposalKind,
    Selector, Store,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

#[test]
fn openai_adapter_sends_rendered_context_and_parses_a_decision() {
    let dir = std::env::temp_dir().join(format!("eja-brain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let goal = store
        .append_simple(Kind::Goal, "original goal", 1, 1)
        .unwrap();
    let old = store
        .append(
            Kind::WorkerReport,
            "task:1",
            "recalled digest",
            1,
            EventMeta::data(
                1,
                serde_json::json!({"summary": "recalled structured evidence"}),
            ),
        )
        .unwrap();
    let evidence = vec![Observation {
        source: LookSource::Log,
        target: String::new(),
        arguments: serde_json::Value::Null,
        selector: Selector::Search {
            query: "old".to_string(),
        },
        result: serde_json::json!({"matches": 1, "events": [old]}),
        error: None,
    }];
    let context =
        project_with_evidence(&store, "revise the goal", None, eja::now_utc(), &evidence).unwrap();
    assert!(render_context(&context).contains("CURRENT INPUT"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("POST /chat/completions"));
        let mut length = None;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
        }
        let mut body = vec![0; length.expect("content length")];
        reader.read_exact(&mut body).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("You are EJA"));
        let input = request["messages"][1]["content"].as_str().unwrap();
        assert!(input.contains(&format!("#{} original goal", goal.id)));
        assert!(input.contains("EVIDENCE"));
        assert!(input.contains("recalled digest"));
        assert!(input.contains("recalled structured evidence"));
        let content = serde_json::json!({
            "reply": "recorded",
            "actions": [{"op": "propose", "kind": "goal", "target_id": goal.id, "text": "replacement goal"}]
        })
        .to_string();
        let body = serde_json::json!({
            "choices": [{"message": {"content": content}}]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });

    let brain = OpenAiBrain::new(format!("http://127.0.0.1:{port}"), "test-key", "test-model");
    let decision = brain.decide(&context);
    assert_eq!(decision.reply, "recorded");
    assert_eq!(
        decision.actions,
        vec![Action::Propose {
            kind: ProposalKind::Goal,
            target_id: Some(goal.id),
            text: "replacement goal".into(),
        }]
    );
    server.join().expect("server");
}

#[test]
fn one_unreadable_action_does_not_discard_the_rest() {
    let raw = r#"{"reply":"go","actions":[{"op":"focus","task_id":1},{"op":"look","source":"node","target":"3","selector":{"select":"last"}}]}"#;
    let decision = eja::adapters::parse_decision(raw);
    assert_eq!(decision.actions.len(), 1, "{:?}", decision.actions);
    assert_eq!(decision.refused.len(), 1, "{:?}", decision.refused);
    assert!(
        decision.refused[0].contains("selector"),
        "{:?}",
        decision.refused
    );
    assert_eq!(decision.reply, "go");
}

#[test]
fn a_plain_reply_is_still_a_finished_decision() {
    let decision = eja::adapters::parse_decision("nothing left to do");
    assert!(decision.actions.is_empty());
    assert_eq!(decision.reply, "nothing left to do");
}

fn capture_request(response: &'static str) -> (String, std::thread::JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut length = None;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
        }
        let mut body = vec![0; length.expect("content length")];
        reader.read_exact(&mut body).unwrap();
        let response_body = response;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(reply.as_bytes()).expect("write");
        serde_json::from_slice(&body).unwrap()
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

#[test]
fn json_mode_is_requested_only_where_a_json_object_is_expected() {
    use eja::adapters::{Control, Model, OpenAiModel};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    let answered = r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let (url, server) = capture_request(answered);
    let control = Control::new(Duration::from_secs(5), Arc::new(AtomicBool::new(false)));
    let model = OpenAiModel::new(url, "k", "m");
    model
        .complete(
            &[serde_json::json!({"role":"user","content":"choose"})],
            &[],
            &eja::aja::Limits::default(),
            &control,
            true,
        )
        .expect("decision call");
    let decision = server.join().expect("server");
    assert_eq!(decision["response_format"]["type"], "json_object");

    // The node's turn acts through tool calls and ends in plain text; asking the
    // provider for a JSON object there is wrong and a provider may reject it.
    let (url, server) = capture_request(answered);
    let model = OpenAiModel::new(url, "k", "m");
    model
        .complete(
            &[serde_json::json!({"role":"user","content":"work"})],
            &[],
            &eja::aja::Limits::default(),
            &control,
            false,
        )
        .expect("node call");
    let node = server.join().expect("server");
    assert!(node.get("response_format").is_none(), "{node}");
}
