use kumochi_protocol::message::ControlToBrain;

use crate::link::{LinkHandle, Session};
use crate::runs::Runs;

/// Clears a dropped presence's pending input and ends its connection, even if a send fails.
pub fn end_link(link: &LinkHandle, session: Option<&Session>, runs: &mut Runs) {
    let sent = link
        .revoke_input_for_dispatch()
        .and_then(|cancel| link.dispatch_input_cancel(&cancel).then(|| cancel.target()));
    runs.cancel_hold();
    let pending = runs.take_hold_cancel();
    if let Some(session) = session {
        if let Some(message) = pending
            && !matches!(message, ControlToBrain::GestureCancel { gesture }
                if sent == Some((gesture, session.generation())))
        {
            let _queued = link.send_control(session, message);
        }
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
