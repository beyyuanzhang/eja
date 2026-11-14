use eja::adapters::Ports;
use eja::{
    open_commitments, turn_at, Action, Assignment, Brain, Decision, EjaContext, Kind, LookSource,
    ProposalKind, Selector, Store, Worker,
};
use std::collections::VecDeque;
use std::sync::Mutex;

struct QueueBrain {
    queue: Mutex<VecDeque<Decision>>,
}

impl QueueBrain {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            queue: Mutex::new(decisions.into()),
        }
    }
}

impl Brain for QueueBrain {
    fn decide(&self, _context: &EjaContext, _: &eja::Control) -> Decision {
        self.queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: String::new(),
                actions: Vec::new(),
                refused: Vec::new(),
            })
    }
}

struct NoWorker;

impl Worker for NoWorker {
    fn execute(&self, _assignment: &Assignment, _: &eja::Control) -> String {
        format!(
            "0 steps, 0 tool calls, 0 artifacts, ended: error\n{}",
            "no worker"
        )
    }
}

fn decision(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.to_string(),
        actions,
        refused: Vec::new(),
    }
}

fn temp_store() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("eja-recall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
fn recall_is_an_id_result_not_text_matching() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let propose = QueueBrain::new(vec![decision(
        "proposed",
        vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "arbitrary old fact".to_string(),
        }],
    )]);
    let proposed = turn_at(
        &store,
        "commit",
        &propose,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("propose");
    let proposal_id = proposed.context.proposals.last().expect("proposal").id;
    let confirm = QueueBrain::new(vec![decision(
        "confirmed",
        vec![Action::Confirm { proposal_id }],
    )]);
    eja::turn_at_with_options(
        &store,
        "confirm",
        &confirm,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
        &eja::TurnOptions {
            approvals: vec![proposal_id],
            ..Default::default()
        },
    )
    .expect("confirm");
    let commitment = open_commitments(&store.events().expect("events"))[0].clone();

    let brain = QueueBrain::new(vec![
        decision(
            "searching",
            vec![Action::Look {
                source: LookSource::Log,
                target: format!("event:{}", commitment.id),
                arguments: serde_json::Value::Null,
                selector: Selector::Whole,
            }],
        ),
        decision("found it", vec![]),
    ]);
    let outcome = turn_at(
        &store,
        "what did we decide?",
        &brain,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("recall");

    assert_eq!(outcome.evidence.len(), 1);
    let events = outcome.evidence[0].result["events"]
        .as_array()
        .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], commitment.id);
    assert_eq!(events[0]["kind"], "commitment");
    assert_eq!(outcome.context.evidence.len(), 1);
    assert!(store
        .events()
        .expect("events")
        .iter()
        .any(|event| event.kind == Kind::Observed));
}
