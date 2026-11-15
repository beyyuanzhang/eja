//! The graph a turn folds: containment, typed relations, cycles and group closure.

mod support;

use eja::{Action, Edge, RelationKind, TaskStatus};
use support::{decision, group, link, task, turn, State};

#[test]
fn parent_and_typed_relation_are_folded_into_the_graph() {
    let state = State::new("graph");
    let outcome = turn(
        &state.store(),
        vec![decision(vec![
            group("long task"),
            task("child task"),
            link(2, 1, RelationKind::PartOf),
        ])],
    );

    assert_eq!(
        outcome.context().relations,
        vec![Edge {
            from: 2,
            to: 1,
            kind: RelationKind::PartOf,
        }]
    );
    // The edge is one event, scoped to its source task, not two task mutations.
    assert!(state
        .events()
        .iter()
        .any(|event| event.kind == eja::Kind::TaskRelation && event.scope == "task:2"));
}

#[test]
fn containment_needs_an_open_group() {
    let state = State::new("containment");
    // A aja is not a container, and a task belongs to one group.
    let outcome = turn(
        &state.store(),
        vec![
            decision(vec![task("solo"), group("first"), group("second")]),
            decision(vec![
                link(1, 1, RelationKind::PartOf),
                link(1, 1, RelationKind::DependsOn),
                link(1, 2, RelationKind::PartOf),
                link(1, 3, RelationKind::PartOf),
            ]),
        ],
    );
    let context = outcome.context();
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("self relation")));
    // Only the first containment is recorded; the second is refused, and neither
    // renamed the aja into a group.
    assert_eq!(
        context.relations,
        vec![Edge {
            from: 1,
            to: 2,
            kind: RelationKind::PartOf,
        }]
    );
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("already belongs to a group")));
}

#[test]
fn relating_the_same_edge_twice_is_one_fact() {
    let state = State::new("idempotent");
    let outcome = turn(
        &state.store(),
        vec![
            decision(vec![group("g"), task("member")]),
            decision(vec![
                link(2, 1, RelationKind::PartOf),
                link(2, 1, RelationKind::PartOf),
            ]),
        ],
    );
    assert_eq!(outcome.context().relations.len(), 1);
    assert_eq!(
        state
            .events()
            .iter()
            .filter(|event| event.kind == eja::Kind::TaskRelation)
            .count(),
        1,
        "a relation stated twice is one fact"
    );
}

#[test]
fn a_group_with_an_active_member_refuses_to_close() {
    let state = State::new("group-close");
    let outcome = turn(
        &state.store(),
        vec![
            decision(vec![group("g"), task("member")]),
            // One decision: a successful relation alone ends the turn.
            decision(vec![
                link(2, 1, RelationKind::PartOf),
                Action::CloseTask {
                    task_id: 1,
                    accepted: true,
                    note: "done".to_string(),
                },
            ]),
        ],
    );
    let context = outcome.context();
    // The member is still open, so the container cannot complete.
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("still has active children")));
    assert_eq!(context.tasks[0].status, TaskStatus::Open);
}

#[test]
fn a_group_closed_unaccepted_is_failed() {
    let state = State::new("group-reject");
    let outcome = turn(
        &state.store(),
        vec![
            decision(vec![group("g")]),
            decision(vec![Action::CloseTask {
                task_id: 1,
                accepted: false,
                note: "not wanted".to_string(),
            }]),
        ],
    );
    let context = outcome.context();
    assert_eq!(context.tasks[0].status, TaskStatus::Failed);
    assert_eq!(
        context.tasks[0].close.as_ref().expect("close").note,
        "not wanted"
    );
}

#[test]
fn a_dependency_cycle_is_refused() {
    let state = State::new("cycle");
    let outcome = turn(
        &state.store(),
        vec![
            decision(vec![task("a"), task("b")]),
            decision(vec![
                link(1, 2, RelationKind::DependsOn),
                link(2, 1, RelationKind::DependsOn),
            ]),
        ],
    );
    let context = outcome.context();
    assert_eq!(context.relations.len(), 1);
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("dependency cycle")));
}
