use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use async_channel::{TryRecvError, bounded};
use kumochi_protocol::message::{AudioSession, ControlToBrain, Incarnation, RunId};

use super::{LinkEvent, LinkHandle, LinkState, Session, prepare_disconnected};

#[test]
fn unavailable_transport_reports_failed_without_retry_or_quit_work() {
    let (events, states) = bounded(1);
    let link = prepare_disconnected(&events).unwrap();
    assert!(matches!(
        states.try_recv(),
        Ok(LinkEvent::State(LinkState::Failed))
    ));
    assert_eq!(link.link_generation(), 0);
    assert!(!link.admits_text(None));
    assert!(!link.can_start_voice_gesture(0));
    assert!(!link.admits_voice(0));
    assert!(!link.retry());
    assert!(!link.retry());
    assert_eq!(states.try_recv().unwrap_err(), TryRecvError::Empty);

    let other = link.clone();
    assert!(link.begin_quit());
    assert!(!other.begin_quit());
    assert!(other.is_quitting());
    other.finish_quit();
    assert!(!other.retry());
    assert_eq!(states.try_recv().unwrap_err(), TryRecvError::Empty);
    drop(events);
    assert_eq!(states.try_recv().unwrap_err(), TryRecvError::Closed);
}

#[test]
fn quit_refuses_every_later_brain_child_claim() {
    let link = LinkHandle::detached();
    assert!(link.begin_quit());
    assert!(!link.shared.spawn.claim());
    assert!(!link.begin_quit());
    assert!(!link.shared.spawn.claim());
}

/// The next event within the patience, or a failure.
fn next_event(events: &async_channel::Receiver<LinkEvent>) -> LinkEvent {
    let by = Instant::now() + Duration::from_secs(5);
    loop {
        match events.try_recv() {
            Ok(event) => return event,
            Err(TryRecvError::Empty) if Instant::now() < by => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("no link event in time: {error:?}"),
        }
    }
}

#[test]
fn a_busy_attempt_waits_for_retry_and_a_quit_with_no_child_is_not_clean() {
    let (events, states) = async_channel::unbounded();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let link = super::start_scripted(events, AudioSession::Legacy, move |_, _, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        Err(crate::platform::LinkError::Busy.into())
    });
    for attempt in 1..=2 {
        if attempt > 1 {
            assert!(link.retry());
        }
        assert!(matches!(
            next_event(&states),
            LinkEvent::State(LinkState::Connecting)
        ));
        assert!(matches!(
            next_event(&states),
            LinkEvent::State(LinkState::Failed)
        ));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(states.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            attempt,
            "one attempt per Retry"
        );
    }
    assert!(link.begin_quit());
    link.finish_quit();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "Quit started no attempt"
    );
    assert_eq!(link.quit_end(), None, "no attempt was live at Quit");
    assert_eq!(states.try_recv().unwrap_err(), TryRecvError::Closed);
}

#[test]
fn a_quit_during_a_failed_attempt_with_no_child_has_no_outcome() {
    let (events, _states) = async_channel::unbounded();
    let (entered, attempting) = std::sync::mpsc::channel();
    let (fail, failing) = std::sync::mpsc::channel::<()>();
    let link = super::start_scripted(events, AudioSession::Legacy, move |_, _, _| {
        let _seen = entered.send(());
        let _released = failing.recv_timeout(Duration::from_secs(10));
        Err(crate::platform::LinkError::Busy.into())
    });
    attempting
        .recv_timeout(Duration::from_secs(10))
        .expect("the attempt started");
    assert!(link.begin_quit());
    drop(fail);
    link.finish_quit();
    assert_eq!(link.quit_end(), None, "a childless attempt has no outcome");
}

#[test]
fn a_quit_during_a_failed_attempt_keeps_what_it_knows_of_its_child() {
    use crate::link::{QuitEnd, QuitOutcome};
    use crate::platform::{Exit, Failed, LinkError, Settled, Unconnected};

    for (child, outcome) in [
        (Unconnected::NotSpawned, QuitOutcome::NotSpawned),
        (Unconnected::Pending, QuitOutcome::SpawnPending),
        (
            Unconnected::Settled(Settled::Reaped(Exit::Code(0))),
            QuitOutcome::Reaped(Exit::Code(0)),
        ),
        (
            Unconnected::Settled(Settled::Unsettled),
            QuitOutcome::Unsettled,
        ),
    ] {
        let (events, _states) = async_channel::unbounded();
        let (entered, attempting) = std::sync::mpsc::channel();
        let (fail, failing) = std::sync::mpsc::channel::<()>();
        let link = super::start_scripted(events, AudioSession::Legacy, move |_, _, _| {
            let _seen = entered.send(());
            let _released = failing.recv_timeout(Duration::from_secs(10));
            Err(Failed {
                error: LinkError::Rejected,
                child: Some(child),
            })
        });
        attempting
            .recv_timeout(Duration::from_secs(10))
            .expect("the attempt started");
        assert!(link.begin_quit());
        drop(fail);
        link.finish_quit();
        assert_eq!(
            link.quit_end(),
            Some(QuitEnd {
                generation: None,
                outcome
            }),
            "{child:?}"
        );
    }
}

#[test]
fn unavailable_transport_does_not_hide_an_undelivered_initial_state() {
    let (events, states) = bounded(1);
    events
        .try_send(LinkEvent::State(LinkState::Connecting))
        .unwrap();
    assert_eq!(
        prepare_disconnected(&events).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    assert!(matches!(
        states.try_recv(),
        Ok(LinkEvent::State(LinkState::Connecting))
    ));
    drop(states);
    assert_eq!(
        prepare_disconnected(&events).unwrap_err().kind(),
        ErrorKind::BrokenPipe
    );
}

#[test]
fn workerless_handle_cannot_send_or_end_another_sessions_control() {
    let (events, _states) = bounded(1);
    let offline = prepare_disconnected(&events).unwrap();
    for queued in [false, true] {
        let (control, incoming) = bounded(1);
        let owner = LinkHandle::detached();
        let mut session = Session::detached(1, Incarnation(1));
        let io = Arc::get_mut(&mut session.io).unwrap();
        io.queued = queued;
        io.control = Some(control.downgrade());
        let stop = ControlToBrain::Stop {
            run: RunId(2),
            revealed: Vec::new(),
        };

        let sent = offline.send_control(&session, stop.clone());
        let received = incoming.try_recv();
        offline.end(&session);
        assert_eq!(
            (sent, received, session.open()),
            (false, Err(TryRecvError::Empty), true),
        );

        assert!(owner.send_control(&session, stop.clone()));
        assert_eq!(incoming.try_recv(), Ok(stop));
        owner.end(&session);
        assert!(!session.open());
    }
}
