mod support;

use eja::{EventMeta, Kind};
use support::State;

#[test]
fn ids_and_turns_continue_across_restart() {
    let fixture = State::new("store");
    let store = fixture.store();
    let first = store
        .append(Kind::User, "eja", "one", 1, EventMeta::new(1))
        .expect("one");
    let second = store
        .append(Kind::User, "eja", "two", 2, EventMeta::new(2))
        .expect("two");
    assert_eq!(first.id, 1);
    assert_eq!(second.id, 2);
    assert_eq!(store.next_turn().expect("turn"), 3);

    let store = fixture.reopen();
    let third = store
        .append(Kind::User, "eja", "three", 7, EventMeta::new(3))
        .expect("three");
    assert_eq!(third.id, 3);
    assert_eq!(store.next_turn().expect("turn"), 8);
}
