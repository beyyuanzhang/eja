use eja::adapters::Ports;
use eja::{
    open_commitments, turn_at, Action, Assignment, Brain, Decision, EjaContext, Kind, ProposalKind,
    Report, Store, Worker,
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
    fn decide(&self, _context: &EjaContext) -> Decision {
        self.queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: String::new(),
                actions: Vec::new(),
            })
    }
}

struct NoWorker;

impl Worker for NoWorker {
    fn execute(&self, _assignment: &Assignment) -> Report {
        Report::failed("no worker")
    }
}

fn decision(reply: &str, actions: Vec<Action>) -> Decision {
    Decision {
        reply: reply.to_string(),
        actions,
    }
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn store() -> Store {
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eja-proposals-{}-{}", std::process::id(), serial));
    let _ = std::fs::remove_dir_all(&dir);
    Store::new(&dir).expect("store")
}

#[test]
fn proposal_does_not_change_identity_until_confirmed() {
    let store = store();
    let propose = QueueBrain::new(vec![decision(
        "proposed",
        vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "answer first".to_string(),
        }],
    )]);
    let proposed = turn_at(
        &store,
        "propose",
        &propose,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("propose");
    assert_eq!(proposed.context.proposals.len(), 1);
    assert!(open_commitments(&store.events().expect("events")).is_empty());

    let proposal_id = proposed.context.proposals[0].id;
    let confirm = QueueBrain::new(vec![decision(
        "confirmed",
        vec![Action::Confirm { proposal_id }],
    )]);
    let confirmed = turn_at(
        &store,
        "confirm",
        &confirm,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("confirm");
    assert!(confirmed.context.proposals.is_empty());
    assert_eq!(open_commitments(&store.events().expect("events")).len(), 1);
}

#[test]
fn a_proposal_cannot_be_confirmed_in_the_same_turn() {
    let store = store();
    let brain = QueueBrain::new(vec![decision(
        "propose and confirm",
        vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: None,
                text: "answer first".to_string(),
            },
            Action::Confirm { proposal_id: 2 },
        ],
    )]);
    turn_at(
        &store,
        "do both",
        &brain,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("turn");
    assert!(open_commitments(&store.events().expect("events")).is_empty());
}

#[test]
fn rejection_leaves_identity_unchanged() {
    let store = store();
    let propose = QueueBrain::new(vec![decision(
        "proposed",
        vec![Action::Propose {
            kind: ProposalKind::Commitment,
            target_id: None,
            text: "answer first".to_string(),
        }],
    )]);
    let proposed = turn_at(
        &store,
        "propose",
        &propose,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("propose");
    let proposal_id = proposed.context.proposals[0].id;
    let reject = QueueBrain::new(vec![decision(
        "rejected",
        vec![Action::Reject {
            proposal_id,
            note: "no".to_string(),
        }],
    )]);
    let rejected = turn_at(
        &store,
        "reject",
        &reject,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("reject");
    assert!(rejected.context.proposals.is_empty());
    assert!(open_commitments(&store.events().expect("events")).is_empty());
}

#[test]
fn replacements_wait_for_confirmation_and_revalidate_the_target() {
    let store = store();
    let old = store
        .append_simple(Kind::Commitment, "original", 1, 1)
        .unwrap();
    let propose = QueueBrain::new(vec![decision(
        "proposed",
        vec![
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: Some(old.id),
                text: "replacement".into(),
            },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: Some(old.id),
                text: "stale replacement".into(),
            },
        ],
    )]);
    let out = turn_at(
        &store,
        "propose",
        &propose,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .unwrap();
    assert_eq!(out.context.proposals.len(), 2);
    assert_eq!(open_commitments(&store.events().unwrap())[0].id, old.id);
    let first = out.context.proposals[0].id;
    let stale = out.context.proposals[1].id;
    let confirm = QueueBrain::new(vec![decision(
        "confirm",
        vec![
            Action::Confirm { proposal_id: first },
            Action::Confirm { proposal_id: stale },
        ],
    )]);
    let out = turn_at(
        &store,
        "confirm",
        &confirm,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .unwrap();
    assert_eq!(out.context.commitments.len(), 1);
    assert_eq!(out.context.commitments[0].text, "replacement");
    assert_eq!(out.context.commitments[0].closes, Some(old.id));
    assert_eq!(out.context.proposals.len(), 1);
    assert_eq!(out.context.proposals[0].id, stale);
    let events = store.events().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == Kind::ProposalConfirmed)
            .count(),
        1
    );
}

#[test]
fn invalid_revision_targets_do_not_create_proposals() {
    let store = store();
    let old = store
        .append_simple(Kind::Commitment, "original", 1, 1)
        .unwrap();
    let brain = QueueBrain::new(vec![decision(
        "invalid",
        vec![
            Action::Propose {
                kind: ProposalKind::Goal,
                target_id: Some(old.id),
                text: "wrong kind".into(),
            },
            Action::Propose {
                kind: ProposalKind::Commitment,
                target_id: Some(999),
                text: "missing target".into(),
            },
            Action::Propose {
                kind: ProposalKind::Correction,
                target_id: None,
                text: "missing target".into(),
            },
        ],
    )]);
    let out = turn_at(
        &store,
        "propose",
        &brain,
        &NoWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .unwrap();
    assert!(out.context.proposals.is_empty());
    assert!(out.context.goal.is_none());
    assert_eq!(out.context.commitments[0].id, old.id);
}
