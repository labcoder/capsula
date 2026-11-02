//! Held deletes: when each is queued, and whether it says it's a resend.

use std::sync::Arc;

use async_channel::{Receiver, bounded};
use capsula_protocol::message::{ControlToBrain, ThreadId};

use super::{LinkHandle, Shared, queue_on};

fn handle() -> LinkHandle {
    let (retry, _retries) = bounded(1);
    let (_alive, done) = bounded(1);
    LinkHandle {
        shared: Arc::new(Shared::default()),
        retry,
        done,
    }
}

/// Queues `handle`'s held deletes on connection `generation`.
fn queue(
    handle: &LinkHandle,
    control: &async_channel::Sender<ControlToBrain>,
    generation: u64,
) -> bool {
    queue_on(control, generation, 2, &mut handle.shared.deletes())
}

fn sent(queue: &Receiver<ControlToBrain>) -> Vec<(u64, bool)> {
    std::iter::from_fn(|| queue.try_recv().ok())
        .map(|message| match message {
            ControlToBrain::DeleteThread {
                thread,
                after_turns,
                resend,
            } => {
                assert_eq!(after_turns, 2);
                (thread.0, resend)
            }
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[test]
fn a_confirmed_delete_is_sent_once_per_connection_and_resent_as_a_resend() {
    let link = handle();
    let (control, queued) = bounded(8);
    link.delete(ThreadId(1));
    assert!(queue(&link, &control, 1));
    assert!(queue(&link, &control, 1));
    assert_eq!(sent(&queued), [(1, false)], "once on its connection");
    link.delete(ThreadId(2));
    assert!(queue(&link, &control, 2));
    assert_eq!(sent(&queued), [(1, true), (2, false)]);
}

#[test]
fn a_resend_stays_one_through_failed_enqueues() {
    let link = handle();
    let (control, queued) = bounded(8);
    link.delete(ThreadId(1));
    assert!(queue(&link, &control, 1));
    sent(&queued);
    let (full, waiting) = bounded(1);
    full.try_send(ControlToBrain::Quitting).expect("room");
    assert!(!queue(&link, &full, 2), "no room");
    assert!(!queue(&link, &full, 2), "still none");
    assert_eq!(waiting.try_recv(), Ok(ControlToBrain::Quitting));
    assert!(queue(&link, &full, 2));
    assert_eq!(sent(&waiting), [(1, true)]);
}

#[test]
fn a_first_attempt_that_found_no_room_is_still_not_a_resend() {
    let link = handle();
    let (full, waiting) = bounded(1);
    full.try_send(ControlToBrain::Quitting).expect("room");
    link.delete(ThreadId(1));
    assert!(!queue(&link, &full, 1));
    waiting.try_recv().expect("the filler");
    assert!(queue(&link, &full, 1));
    assert_eq!(sent(&waiting), [(1, false)]);
}

#[test]
fn an_unsent_resend_is_dropped_once_its_answer_arrives() {
    let link = handle();
    let (control, queued) = bounded(8);
    link.delete(ThreadId(1));
    assert!(queue(&link, &control, 1));
    sent(&queued);
    let (full, waiting) = bounded(1);
    full.try_send(ControlToBrain::Quitting).expect("room");
    assert!(!queue(&link, &full, 2));
    link.settle_delete(ThreadId(1));
    waiting.try_recv().expect("the filler");
    assert!(queue(&link, &full, 2));
    assert_eq!(sent(&waiting), []);
}

#[test]
fn a_fresh_confirm_after_its_answer_is_not_a_resend() {
    let link = handle();
    let (control, queued) = bounded(8);
    link.delete(ThreadId(1));
    assert!(queue(&link, &control, 1));
    link.settle_delete(ThreadId(1));
    link.delete(ThreadId(1));
    assert!(queue(&link, &control, 1));
    assert!(queue(&link, &control, 2));
    assert_eq!(sent(&queued), [(1, false), (1, false), (1, true)]);
}

#[test]
fn deletes_stop_at_a_full_queue_and_resume_in_order() {
    let link = handle();
    let (control, queued) = bounded(2);
    for thread in 1..=3 {
        link.delete(ThreadId(thread));
    }
    assert!(!queue(&link, &control, 1));
    assert_eq!(sent(&queued), [(1, false), (2, false)]);
    assert!(queue(&link, &control, 1));
    assert_eq!(sent(&queued), [(3, false)]);
}
