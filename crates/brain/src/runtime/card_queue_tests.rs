use std::sync::Arc;
use std::time::Instant;

use kumochi_protocol::frame;
use kumochi_protocol::message::{
    ActionState, ContentHash, DataToShell, DraftId, IssueBody, IssueCard, IssueCardsStatus,
    IssueTitle, StateVersion, ThreadId,
};
use kumochi_scene::text::Bounded;

use super::DataQueue;
use crate::coordinator::{CardRead, Cards};
use crate::store::Generation;

fn batch(queue: &mut DataQueue) -> (Arc<Cards>, Vec<CardRead>) {
    let cards = queue
        .admit_cards(
            1,
            7,
            &Bounded::new(vec![DraftId(1), DraftId(2), DraftId(1)]).unwrap(),
        )
        .unwrap();
    let rows = cards
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let thread = ThreadId(10 + u64::try_from(index).unwrap());
            assert!(source.bind(thread, Generation(0)));
            CardRead {
                card: IssueCard {
                    draft: source.draft,
                    thread,
                    repo: None,
                    account: None,
                    title: IssueTitle::new("private-card-marker").unwrap(),
                    body: IssueBody::new("private-card-marker").unwrap(),
                    hash: ContentHash([3; 32]),
                    approval: None,
                    version: StateVersion(1),
                    state: ActionState::Draft,
                },
                generation: Generation(0),
                source: Arc::clone(source),
            }
        })
        .collect();
    (cards, rows)
}

fn pop(queue: &mut DataQueue) -> DataToShell {
    let bytes = queue.pop(Instant::now()).unwrap();
    frame::decode(&bytes[frame::HEADER_LEN..]).unwrap()
}

#[test]
fn cards_share_page_slots_and_release_only_their_terminal() {
    let mut queue = DataQueue::new(10000);
    let (cards, rows) = batch(&mut queue);
    assert_eq!(
        cards
            .sources
            .iter()
            .map(|source| source.draft)
            .collect::<Vec<_>>(),
        [DraftId(1), DraftId(2)]
    );
    assert!(queue.admit_page(1, 7).is_none());
    for id in 8..11 {
        assert!(queue.admit_page(1, id).is_some());
    }
    assert!(queue.admit_page(1, 11).is_none());
    assert!(queue.push_cards(&cards, Ok(rows)));
    assert!(
        matches!(pop(&mut queue), DataToShell::IssueCardRead { request: 7, card } if card.draft == DraftId(1))
    );
    queue.wrote();
    assert!(queue.admit_page(1, 11).is_none());
    cards.sources[1].delete();
    assert_eq!(
        pop(&mut queue),
        DataToShell::IssueCardsDone {
            request: 7,
            status: IssueCardsStatus::Complete
        }
    );
    assert!(queue.admit_page(1, 11).is_none());
    queue.wrote();
    assert!(queue.admit_page(1, 11).is_some());
    queue.wrote();
    assert!(queue.admit_page(1, 12).is_none());
}

#[test]
fn all_pruned_empty_and_failed_batches_keep_their_terminal_and_lease() {
    for status in [IssueCardsStatus::Complete, IssueCardsStatus::Failed] {
        let mut queue = DataQueue::new(10000);
        let (cards, rows) = batch(&mut queue);
        for source in &cards.sources {
            source.delete();
        }
        let reply = match status {
            IssueCardsStatus::Complete => Ok(rows),
            IssueCardsStatus::Failed => Err(()),
        };
        assert!(queue.push_cards(&cards, reply));
        assert_eq!(
            pop(&mut queue),
            DataToShell::IssueCardsDone { request: 7, status }
        );
        assert!(queue.has_reads());
        queue.wrote();
        assert!(!queue.has_reads());
        assert_eq!(queue.pop(Instant::now()), None);
    }
    let mut queue = DataQueue::new(100);
    let cards = queue.admit_cards(1, 3, &Bounded::default()).unwrap();
    assert!(queue.push_cards(&cards, Ok(Vec::new())));
    assert_eq!(
        pop(&mut queue),
        DataToShell::IssueCardsDone {
            request: 3,
            status: IssueCardsStatus::Complete
        }
    );
    queue.wrote();
    assert!(!queue.has_reads());
}

#[test]
fn deletion_after_queueing_or_during_writing_cannot_revive_a_source() {
    let mut queue = DataQueue::new(10000);
    let (cards, rows) = batch(&mut queue);
    assert!(queue.push_cards(&cards, Ok(rows)));
    cards.sources[0].delete();
    assert!(!cards.sources[0].bind(ThreadId(10), Generation(0)));
    assert!(
        matches!(pop(&mut queue), DataToShell::IssueCardRead { card, .. } if card.draft == DraftId(2))
    );
    cards.sources[1].delete();
    assert!(queue.reads_ended());
    queue.end_reads();
    assert!(cards.key.control.check().is_err());
    assert!(!queue.accepts_cards(&cards));
}

#[test]
fn card_carriers_never_print_their_content() {
    let mut queue = DataQueue::new(10000);
    let (cards, rows) = batch(&mut queue);
    let input = crate::coordinator::Input::CardsRead {
        cards: Arc::clone(&cards),
        reply: Ok(rows.clone()),
    };
    let effect = crate::coordinator::Effect::Cards {
        cards,
        reply: Ok(rows),
    };
    let debug = format!("{input:?} {effect:?}");
    assert!(!debug.contains("private-card-marker"));
    assert!(!debug.contains(&format!("{:?}", b"private-card-marker")));
}

#[test]
fn an_oversized_completion_cannot_queue_a_partial_batch() {
    let mut queue = DataQueue::new(10000);
    let (cards, mut rows) = batch(&mut queue);
    rows.push(rows[0].clone());
    assert!(!queue.push_cards(&cards, Ok(rows)));
    assert_eq!(queue.pop(Instant::now()), None);
    assert!(queue.has_reads());
    queue.end_reads();
    assert!(cards.key.control.check().is_err());
}
