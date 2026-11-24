use kumochi_protocol::message::ControlToBrain;

use crate::link::{InputCancel, LinkHandle, Session};
use crate::runs::Runs;

/// Revokes tracked input and returns its cancel only if the original outbox accepted it.
pub fn cancel_tracked_input(link: &LinkHandle) -> Option<InputCancel> {
    link.revoke_input_for_dispatch()
        .filter(|cancel| link.dispatch_input_cancel(cancel))
}

/// Consumes the Runs cancel, sending it unless the same cancel was already queued.
pub fn drain_hold_cancel(
    link: &LinkHandle,
    session: Option<&Session>,
    runs: &mut Runs,
    queued: Option<&InputCancel>,
) -> bool {
    runs.take_hold_cancel().is_none_or(|message| {
        session.is_some_and(|session| {
            matches!(message, ControlToBrain::GestureCancel { gesture }
                if queued.is_some_and(|cancel| cancel.target() == (gesture, session.generation())))
                || link.send_control(session, message)
        })
    })
}

/// Clears a dropped presence's pending input and ends its connection, even if a send fails.
pub fn end_link(link: &LinkHandle, session: Option<&Session>, runs: &mut Runs) {
    let queued = cancel_tracked_input(link);
    runs.cancel_hold();
    let _queued = drain_hold_cancel(link, session, runs, queued.as_ref());
    if let Some(session) = session {
        link.end(session);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use async_channel::{Receiver, TryRecvError};
    use kumochi_protocol::message::{GestureId, Incarnation, ThreadId};

    use super::*;
    use crate::platform::microphone::MicError;

    const HOLD: GestureId = GestureId(7);

    fn recognized_hold() -> (LinkHandle, Session, Runs, Receiver<ControlToBrain>) {
        let (mut link, _data) = LinkHandle::recording();
        let controls = link.record_controls();
        let session = Session::detached(1, Incarnation(1));
        let mut runs = Runs::default();
        runs.met(Incarnation(1));
        runs.connected(session.generation());
        runs.settings(ThreadId(1));
        let offer = runs.begin_hold(HOLD, None).unwrap();
        assert!(link.send_control(&session, offer.clone()));
        assert_eq!(controls.try_recv(), Ok(offer));
        (link, session, runs, controls)
    }

    #[test]
    fn explicit_cancel_queues_once_and_keeps_the_connection_open() {
        let (link, session, mut runs, controls) = recognized_hold();
        let token = link.microphone_gate().arm(HOLD).unwrap();
        link.track_input(HOLD);

        let queued = cancel_tracked_input(&link);
        assert!(!link.input_is(HOLD));
        assert_eq!(
            token.mode(&AtomicBool::new(true), Instant::now()),
            Err(MicError::Cancelled)
        );
        runs.cancel_hold();
        assert!(drain_hold_cancel(
            &link,
            Some(&session),
            &mut runs,
            queued.as_ref()
        ));
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(runs.take_hold_cancel(), None);
        assert!(session.open());
    }

    #[test]
    fn explicit_cancel_drains_runs_if_the_first_enqueue_was_refused() {
        let (mut link, session, mut runs, _) = recognized_hold();
        let controls = link.record_controls_bounded();
        let filler = ControlToBrain::GestureCancel {
            gesture: GestureId(9),
        };
        for _ in 0..controls.capacity().unwrap() {
            assert!(link.send_control(&session, filler.clone()));
        }
        link.track_input(HOLD);

        let queued = cancel_tracked_input(&link);
        assert!(!link.input_is(HOLD));
        while let Ok(message) = controls.try_recv() {
            assert_eq!(message, filler);
        }
        runs.cancel_hold();
        assert!(drain_hold_cancel(
            &link,
            Some(&session),
            &mut runs,
            queued.as_ref()
        ));
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_cancel_queued_on_another_generation_does_not_hide_the_runs_cancel() {
        let (link, _, _, controls) = recognized_hold();
        let session = Session::detached(2, Incarnation(2));
        let mut runs = Runs::default();
        runs.met(Incarnation(2));
        runs.connected(session.generation());
        runs.settings(ThreadId(1));
        runs.begin_hold(HOLD, None).unwrap();
        link.track_input(HOLD);

        let queued = cancel_tracked_input(&link);
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        runs.cancel_hold();
        assert!(drain_hold_cancel(
            &link,
            Some(&session),
            &mut runs,
            queued.as_ref()
        ));
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_tracked_pending_hold_is_cancelled_once_when_presence_drops() {
        let (link, session, mut runs, controls) = recognized_hold();
        let token = link.microphone_gate().arm(HOLD).unwrap();
        link.track_input(HOLD);

        end_link(&link, Some(&session), &mut runs);
        end_link(&link, Some(&session), &mut runs);
        assert!(!session.open());
        assert!(!link.input_is(HOLD));
        assert_eq!(
            token.mode(&AtomicBool::new(true), Instant::now()),
            Err(MicError::Cancelled)
        );
        assert_eq!(runs.take_hold_cancel(), None);
        drop(link);
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn a_distinct_runs_cancel_already_queued_is_not_lost_on_drop() {
        let (link, session, mut runs, controls) = recognized_hold();
        let tracked = GestureId(9);
        runs.cancel_hold();
        link.track_input(tracked);

        end_link(&link, Some(&session), &mut runs);
        drop(link);
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: tracked })
        );
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn a_refused_cancel_does_not_leave_local_input_or_the_connection_open() {
        let (link, session, mut runs, controls) = recognized_hold();
        let token = link.microphone_gate().arm(HOLD).unwrap();
        link.track_input(HOLD);
        controls.close();

        end_link(&link, Some(&session), &mut runs);
        assert!(!session.open());
        assert!(!link.input_is(HOLD));
        assert_eq!(
            token.mode(&AtomicBool::new(true), Instant::now()),
            Err(MicError::Cancelled)
        );
        assert_eq!(runs.take_hold_cancel(), None);
        assert_eq!(controls.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn a_stale_input_claim_does_not_suppress_the_current_runs_cancel() {
        let (link, session, mut runs, controls) = recognized_hold();
        link.track_input_on(HOLD, 0);

        end_link(&link, Some(&session), &mut runs);
        drop(link);
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::GestureCancel { gesture: HOLD })
        );
        assert_eq!(controls.try_recv(), Err(TryRecvError::Closed));
    }
}
