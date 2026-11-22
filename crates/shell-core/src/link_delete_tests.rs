//! Held deletes: when each is queued, and whether it says it's a resend.

use std::sync::Arc;

use async_channel::{Receiver, bounded};
#[cfg(windows)]
use kumochi_protocol::message::GestureId;
use kumochi_protocol::message::{ControlToBrain, ThreadId};

use super::{LinkHandle, Shared, queue_on};

fn handle() -> LinkHandle {
    let (retry, _retries) = bounded(1);
    let (_alive, done) = bounded(1);
    LinkHandle {
        shared: Arc::new(Shared::default()),
        retry,
        done,
        recording: None,
        control_recording: None,
    }
}

#[test]
#[cfg(windows)]
fn quit_admission_is_single_use_for_a_connected_transport() {
    let link = handle();
    let name = format!(
        r"\\.\pipe\{}-test-{}-settings-quit",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    let (pipe, _peer) = crate::platform::test_pair(&name).expect("pipe pair");
    let (control, queued) = bounded(4);
    let (data, _data) = bounded(1);
    *link.shared.outbox.lock().expect("outbox") = Some(super::Outbox {
        generation: 1,
        control,
        data,
        pipe: Arc::new(pipe),
        submits: std::sync::atomic::AtomicU32::new(0),
        closing: Arc::default(),
    });
    link.delete(ThreadId(8));
    let gesture = GestureId(7);
    link.track_input(gesture);
    let held = link.shared.outbox.lock().expect("outbox");
    let (finished, result) = std::sync::mpsc::channel();
    let worker = {
        let link = link.clone();
        std::thread::spawn(move || finished.send(link.begin_quit()).expect("Quit observer"))
    };
    assert_eq!(result.recv_timeout(super::fixture::PATIENCE), Ok(true));
    drop(held);
    worker.join().unwrap();
    assert!(!link.input_is(gesture));
    link.clear_input(gesture);
    link.track_input(GestureId(9));
    assert!(!link.input_is(GestureId(9)));
    assert!(!link.cancel_input());
    assert!(link.shared.tell_quit());
    assert!(link.shared.tell_quit());
    assert!(!link.begin_quit());
    assert!(!link.begin_quit());
    assert_eq!(
        queued.try_recv(),
        Ok(ControlToBrain::GestureCancel { gesture })
    );
    assert_eq!(
        queued.try_recv(),
        Ok(ControlToBrain::DeleteThread {
            thread: ThreadId(8),
            after_turns: 0,
            resend: false
        })
    );
    assert_eq!(queued.try_recv(), Ok(ControlToBrain::Quitting));
    assert_eq!(queued.try_recv(), Err(async_channel::TryRecvError::Empty));
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
#[cfg(windows)]
fn an_aborted_outbox_refuses_quit_even_after_capacity_returns() {
    let install = |link: &LinkHandle, label, generation| {
        let (peer, pipe) = super::fixture::pair(label);
        let pipe = Arc::new(pipe);
        let (control, queued) = bounded(2);
        let (data, _data) = bounded(1);
        *link.shared.outbox.lock().expect("outbox") = Some(super::Outbox {
            generation,
            control: control.clone(),
            data,
            pipe: Arc::clone(&pipe),
            submits: std::sync::atomic::AtomicU32::new(0),
            closing: Arc::default(),
        });
        (peer, pipe, control, queued)
    };
    for cancel_during_quit in [true, false] {
        let link = handle();
        let (_peer, pipe, control, queued) = install(&link, "quit-cancel-full", 1);
        link.delete(ThreadId(8));
        let gesture = GestureId(7);
        link.track_input(gesture);
        control.try_send(ControlToBrain::Pong).unwrap();
        control.try_send(ControlToBrain::Pong).unwrap();
        if cancel_during_quit {
            assert!(link.begin_quit());
            assert!(!link.shared.tell_quit(), "the cancel does not fit");
        } else {
            assert_eq!(link.cancel_input_gesture(), Some(gesture));
        }
        let use_pipe = kumochi_protocol::frame::write(
            &mut &*pipe,
            &ControlToBrain::Pong,
            kumochi_protocol::frame::Pipe::Control,
        );
        assert!(matches!(
            use_pipe,
            Err(kumochi_protocol::frame::FrameError::Io(error))
                if error.raw_os_error() == Some(windows::Win32::Foundation::ERROR_OPERATION_ABORTED.to_hresult().0)
        ));
        assert_eq!(queued.try_recv(), Ok(ControlToBrain::Pong));
        assert_eq!(queued.try_recv(), Ok(ControlToBrain::Pong));
        if !cancel_during_quit {
            assert!(link.begin_quit());
        }
        assert!(
            !link.shared.tell_quit(),
            "restored capacity cannot revive an aborted pipe"
        );
        assert_eq!(queued.try_recv(), Err(async_channel::TryRecvError::Empty));
        assert!(!link.shared.tell_quit());
        assert_eq!(queued.try_recv(), Err(async_channel::TryRecvError::Empty));
        assert_eq!(link.shared.deletes()[0].sent_on, None);
        assert!(
            !link
                .shared
                .quit_sent
                .load(std::sync::atomic::Ordering::Acquire)
        );

        let (_fresh_peer, _fresh_pipe, _fresh_control, fresh) =
            install(&link, "quit-cancel-fresh", 2);
        assert!(
            link.shared.tell_quit(),
            "a fresh outbox has its own failure state"
        );
        assert_eq!(
            fresh.try_recv(),
            Ok(ControlToBrain::DeleteThread {
                thread: ThreadId(8),
                after_turns: 0,
                resend: false
            })
        );
        assert_eq!(fresh.try_recv(), Ok(ControlToBrain::Quitting));
        assert!(link.shared.tell_quit());
        assert_eq!(fresh.try_recv(), Err(async_channel::TryRecvError::Empty));
    }
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
