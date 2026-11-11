use super::*;
use crate::coordinator::PageKey;
use crate::store::actions_tests::{body, opened, thread, title};
use crate::test_support::Scratch;
use kumochi_protocol::message::{ActionState, ApprovalRev, ContentHash, DraftId, StateVersion};
use kumochi_scene::text::Bounded;

#[test]
fn snapshot_cards_reconstruct_exact_values_in_first_requested_order() {
    let scratch = Scratch::new("card-snapshot");
    let mut store = opened(&scratch);
    let owner = thread(&mut store);
    let first = store
        .create_draft(owner, &title("first"), &body("body one"), None, None)
        .unwrap();
    let second = store
        .create_draft(owner, &title("second"), &body("body two"), None, None)
        .unwrap();
    let cards = Arc::new(Cards::new(
        PageKey::for_tests(1, 7),
        &Bounded::new(vec![
            second.draft,
            DraftId(u64::MAX),
            first.draft,
            second.draft,
        ])
        .unwrap(),
    ));
    store.side.register_cards(&cards).unwrap();
    let snapshot = Snapshot::begin(&store.conn).unwrap();
    let rows = snapshot.cards(&cards).unwrap();
    snapshot.end().unwrap();
    assert_eq!(
        rows.iter().map(|row| row.card.draft).collect::<Vec<_>>(),
        [second.draft, first.draft]
    );
    assert_eq!(rows[0].card.title, title("second"));
    assert_eq!(rows[0].card.body, body("body two"));
    assert_eq!(
        rows[0].card.hash,
        ContentHash([
            185, 126, 43, 164, 103, 146, 160, 203, 196, 22, 174, 40, 238, 31, 232, 99, 108, 190,
            105, 143, 47, 188, 161, 78, 22, 181, 192, 98, 146, 173, 175, 150
        ])
    );
    assert_eq!(rows[0].card.approval, Some(ApprovalRev(2)));
    assert_eq!(rows[0].card.version, StateVersion(1));
    assert_eq!(rows[0].card.state, ActionState::Draft);
    assert!(rows.iter().all(CardRead::live));
    assert!(!cards.sources[0].live(owner, crate::store::Generation(1)));
    store.begin_delete(owner).unwrap();
    assert!(rows.iter().all(|row| !row.live()));
    assert!(
        cards
            .sources
            .iter()
            .all(|source| !source.bind(owner, crate::store::Generation(0)))
    );
}

#[test]
fn deletion_before_snapshot_prunes_only_its_sources_including_unbound_ones() {
    let scratch = Scratch::new("card-delete-before-read");
    let mut store = opened(&scratch);
    let deleted = thread(&mut store);
    let retained = thread(&mut store);
    let first = store
        .create_draft(deleted, &title("gone"), &body("old"), None, None)
        .unwrap();
    let second = store
        .create_draft(retained, &title("stay"), &body("current"), None, None)
        .unwrap();
    let cards = Arc::new(Cards::new(
        PageKey::for_tests(1, 8),
        &Bounded::new(vec![first.draft, second.draft]).unwrap(),
    ));
    store.side.register_cards(&cards).unwrap();
    store.begin_delete(deleted).unwrap();
    let snapshot = Snapshot::begin(&store.conn).unwrap();
    let rows = snapshot.cards(&cards).unwrap();
    snapshot.end().unwrap();
    assert_eq!(
        rows.iter().map(|row| row.card.draft).collect::<Vec<_>>(),
        [second.draft]
    );
    assert!(rows[0].live());
    assert!(!cards.sources[0].bind(deleted, crate::store::Generation(0)));
}
