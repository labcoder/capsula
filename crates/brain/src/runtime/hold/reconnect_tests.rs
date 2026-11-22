use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use super::*;
use kumochi_protocol::message::GestureId;

const KEY: HoldKey = HoldKey {
    epoch: 1,
    gesture: GestureId(7),
};

fn debt(state: u8) -> (Holds, Arc<AtomicU8>) {
    let state = Arc::new(AtomicU8::new(state));
    let peer = ShellProcess::fixture(10, Arc::clone(&state));
    let mut holds = Holds::default();
    holds.debt = Some(KEY);
    holds.debt_peer = Some(peer.clone());
    holds.peer = Some(peer);
    (holds, state)
}

#[test]
fn reconnect_clears_old_device_debt_only_after_confirmed_peer_exit() {
    for state in [0, 2] {
        let (mut holds, old) = debt(state);
        holds.lost();
        holds.connected(ShellProcess::fixture(20, Arc::new(AtomicU8::new(0))));
        holds.opened();
        assert_eq!(holds.debt, Some(KEY));
        assert!(holds.debt_peer.is_some());
        old.store(1, Ordering::Release);
        holds.connected(ShellProcess::fixture(20, Arc::new(AtomicU8::new(0))));
        assert_eq!(holds.debt, None);
        assert!(holds.debt_peer.is_none());
    }
}

#[test]
fn retained_process_identity_never_treats_pid_reuse_as_the_same_live_peer() {
    let old = ShellProcess::fixture(10, Arc::new(AtomicU8::new(1)));
    let reused = ShellProcess::fixture(10, Arc::new(AtomicU8::new(0)));
    assert!(!old.same_live_process(&reused).unwrap());
}

#[tokio::test]
async fn stop_acceptance_does_not_borrow_another_authenticated_process_receipt() {
    use crate::runtime::data::DataQueue;
    use crate::tap::Tap;
    use crate::test_support::{self, Scratch};
    use std::cell::RefCell;
    let scratch = Scratch::new("hold-stop-peer-acceptance");
    let mut brain = test_support::brain(&scratch, "");
    let (holds, old) = debt(0);
    brain.holds = holds;
    brain.epoch = 2;
    let pending = brain.config.stage(SettingsOp::SetHoldToTalk(true)).unwrap();
    pending.write().unwrap();
    brain.config.commit(pending);
    let (out, mut output) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    for (peer, stopped, next, reason) in [
        (20, KEY.gesture, GestureId(10), HoldFailure::Unavailable),
        (10, GestureId(8), GestureId(11), HoldFailure::Unavailable),
        (10, KEY.gesture, GestureId(12), HoldFailure::NotLoaded),
        (10, KEY.gesture, GestureId(13), HoldFailure::NotLoaded),
    ] {
        let state = if peer == 10 {
            Arc::clone(&old)
        } else {
            Arc::new(AtomicU8::new(0))
        };
        brain.connected_shell(ShellProcess::fixture(peer, state));
        for (input, expected) in [
            (
                ControlToBrain::VoiceHoldStopped { gesture: stopped },
                ControlToShell::VoiceHoldStopAccepted { gesture: stopped },
            ),
            (
                ControlToBrain::VoiceHoldRecognized {
                    gesture: next,
                    thread: None,
                    selection: 1,
                },
                ControlToShell::VoiceHoldStatus {
                    gesture: next,
                    status: HoldStatus::Failed(reason),
                },
            ),
        ] {
            assert!(brain.hold_control(&input, &mut link).unwrap());
            let bytes = output.try_recv().unwrap();
            assert_eq!(
                kumochi_protocol::frame::decode::<ControlToShell>(
                    &bytes[kumochi_protocol::frame::HEADER_LEN..]
                )
                .unwrap(),
                expected
            );
        }
        if reason == HoldFailure::NotLoaded {
            assert!(
                brain.holds.debt_peer.is_none(),
                "the receipt released its owner"
            );
        }
        assert!(brain.holds.pending.is_none());
        assert_eq!(brain.coordinator.current_run(), None);
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }
    brain.close().await;
}

#[tokio::test]
async fn unreadable_shell_process_leaves_the_receipt_unacknowledged() {
    use crate::runtime::data::DataQueue;
    use crate::tap::Tap;
    use crate::test_support::{self, Scratch};
    use std::cell::RefCell;
    let scratch = Scratch::new("hold-stop-unreadable-peer");
    let mut brain = test_support::brain(&scratch, "");
    let (holds, old) = debt(0);
    brain.holds = holds;
    brain.epoch = 2;
    brain.connected_shell(ShellProcess::fixture(10, Arc::clone(&old)));
    let (out, mut output) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let stop = ControlToBrain::VoiceHoldStopped {
        gesture: KEY.gesture,
    };
    old.store(2, Ordering::Release);
    assert!(brain.hold_control(&stop, &mut link).unwrap());
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(brain.holds.debt, Some(KEY));
    old.store(0, Ordering::Release);
    assert!(brain.hold_control(&stop, &mut link).unwrap());
    let bytes = output.try_recv().unwrap();
    assert_eq!(
        kumochi_protocol::frame::decode::<ControlToShell>(
            &bytes[kumochi_protocol::frame::HEADER_LEN..]
        )
        .unwrap(),
        ControlToShell::VoiceHoldStopAccepted {
            gesture: KEY.gesture
        }
    );
    assert_eq!(brain.holds.debt, None);
    brain.close().await;
}
