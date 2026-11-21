use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use kumochi_protocol::message::ControlToBrain;

use crate::platform::speaker_cpal::tests::FakePlayback;

use super::super::worker::{
    Command,
    tests::{RUN, open_actor},
};
use super::*;

const WAIT: Duration = Duration::from_secs(5);

struct Device {
    _playback: FakePlayback,
    plays: Arc<AtomicUsize>,
    dispose: Option<(SyncSender<()>, Receiver<()>)>,
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Some((dropping, finish)) = &self.dispose
            && dropping.send(()).is_ok()
        {
            finish.recv_timeout(WAIT).unwrap();
        }
    }
}

fn power(device: &Device, power: Power) -> bool {
    if power == Power::Playing {
        device.plays.fetch_add(1, Ordering::Relaxed);
    }
    device.dispose.is_none()
}

#[test]
fn stopped_open_disposes_without_play_and_excludes_other_opens_until_drop() {
    static FIRST_POOL: AtomicBool = AtomicBool::new(false);
    static NEXT_POOL: AtomicBool = AtomicBool::new(false);
    let _serial = serialize_playback();
    let (entered, first_entered) = mpsc::sync_channel(1);
    let (release, finish) = mpsc::sync_channel(1);
    let (dropping, starts_drop) = mpsc::sync_channel(1);
    let (release_drop, finish_drop) = mpsc::sync_channel(1);
    let plays = Arc::new(AtomicUsize::new(0));
    let first_plays = Arc::clone(&plays);
    let mut dispose = Some((dropping, finish_drop));
    let (first, first_reply) = spawn_with(
        thread::current(),
        Instant::now(),
        move |control, _| {
            entered.send(()).unwrap();
            finish.recv_timeout(WAIT).unwrap();
            let (transport, playback) = FakePlayback::open(Arc::clone(control), &FIRST_POOL);
            Ok((
                Device {
                    _playback: playback,
                    plays: Arc::clone(&first_plays),
                    dispose: dispose.take(),
                },
                transport,
            ))
        },
        power,
    )
    .unwrap();

    let attempted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&attempted);
    let next_plays = Arc::clone(&plays);
    let (second, second_reply) = spawn_with(
        thread::current(),
        Instant::now(),
        move |control, _| {
            count.fetch_add(1, Ordering::Relaxed);
            let (transport, playback) = FakePlayback::open(Arc::clone(control), &NEXT_POOL);
            Ok((
                Device {
                    _playback: playback,
                    plays: Arc::clone(&next_plays),
                    dispose: None,
                },
                transport,
            ))
        },
        power,
    )
    .unwrap();

    let control = Arc::<Control>::default();
    first.send(Request::Open(1, Arc::clone(&control))).unwrap();
    first_entered.recv_timeout(WAIT).unwrap();
    second.send(Request::Open(2, Arc::default())).unwrap();
    assert!(matches!(
        second_reply.recv_timeout(WAIT),
        Ok(Reply::Opened(2, None))
    ));

    control.stop();
    release.send(()).unwrap();
    starts_drop.recv_timeout(WAIT).unwrap();
    second.send(Request::Open(3, Arc::default())).unwrap();
    let during_drop = second_reply.recv_timeout(WAIT);
    release_drop.send(()).unwrap();
    assert!(matches!(
        first_reply.recv_timeout(WAIT),
        Ok(Reply::Opened(1, None))
    ));
    assert!(matches!(during_drop, Ok(Reply::Opened(3, None))));
    assert_eq!(attempted.load(Ordering::Relaxed), 0);
    assert_eq!(
        plays.load(Ordering::Relaxed),
        0,
        "a stopped open must not call play"
    );

    let allowed = Arc::<Control>::default();
    allowed.inhibit();
    second.send(Request::Open(4, allowed)).unwrap();
    let Ok(Reply::Opened(4, Some(transport))) = second_reply.recv_timeout(WAIT) else {
        panic!("an inhibited, uncancelled stream can still start silently");
    };
    assert_eq!(attempted.load(Ordering::Relaxed), 1);
    assert_eq!(plays.load(Ordering::Relaxed), 1);
    drop(transport);
    second.send(Request::Retire(4)).unwrap();
    assert!(matches!(
        second_reply.recv_timeout(WAIT),
        Ok(Reply::Retired(4))
    ));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one device lifecycle across the worker and both backend contenders"
)]
fn detached_worker_cannot_give_a_successor_its_pending_native_permit() {
    static LATE_POOL: AtomicBool = AtomicBool::new(false);
    static NEXT_POOLS: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];

    let _serial = serialize_playback();
    let (reports, receipts) = mpsc::channel();
    let actor = open_actor(move |message| reports.send(message).is_ok());
    let (entered, pending) = mpsc::sync_channel(1);
    let (finish_open, opening) = mpsc::sync_channel(1);
    let (dropping, starts_drop) = mpsc::sync_channel(1);
    let (finish_drop, disposing) = mpsc::sync_channel(1);
    let (exited, backend_done) = mpsc::sync_channel(1);
    let owner = actor.owner.thread().clone();
    let plays = Arc::new(AtomicUsize::new(0));
    let old_plays = Arc::clone(&plays);
    let backend = thread::spawn(move || {
        let mut dispose = Some((dropping, disposing));
        serve(
            &actor.requests,
            &actor.replies,
            &owner,
            Instant::now(),
            move |control, _| {
                entered.send(Arc::clone(control)).unwrap();
                opening.recv_timeout(WAIT).unwrap();
                let (transport, playback) = FakePlayback::open(Arc::clone(control), &LATE_POOL);
                Ok((
                    Device {
                        _playback: playback,
                        plays: Arc::clone(&old_plays),
                        dispose: dispose.take(),
                    },
                    transport,
                ))
            },
            power,
        );
        exited.send(()).unwrap();
    });
    actor.input.command(Command::Expect).unwrap();
    let old_control = pending.recv_timeout(WAIT).unwrap();
    actor.input.command(Command::Stop).unwrap();
    assert_eq!(
        receipts.recv_timeout(WAIT),
        Ok(ControlToBrain::SpeechStopped {
            run: RUN,
            cursor: None
        })
    );
    assert_eq!(old_control.fenced_handoff(), Some(0));
    drop(actor.input);
    assert_eq!(actor.done.recv_timeout(WAIT), Ok(()));
    actor.owner.join().unwrap();
    assert_eq!(backend_done.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(starts_drop.try_recv(), Err(mpsc::TryRecvError::Empty));

    let next_plays = Arc::clone(&plays);
    let mut pools = NEXT_POOLS.iter();
    let (next, results) = spawn_with(
        thread::current(),
        Instant::now(),
        move |control, _| {
            let (transport, playback) =
                FakePlayback::open(Arc::clone(control), pools.next().unwrap());
            Ok((
                Device {
                    _playback: playback,
                    plays: Arc::clone(&next_plays),
                    dispose: None,
                },
                transport,
            ))
        },
        power,
    )
    .unwrap();
    next.send(Request::Open(2, Arc::default())).unwrap();
    assert!(matches!(
        results.recv_timeout(WAIT),
        Ok(Reply::Opened(2, None))
    ));
    finish_open.send(()).unwrap();
    assert_eq!(starts_drop.recv_timeout(WAIT), Ok(()));
    assert_eq!(backend_done.try_recv(), Err(mpsc::TryRecvError::Empty));
    next.send(Request::Open(3, Arc::default())).unwrap();
    assert!(matches!(
        results.recv_timeout(WAIT),
        Ok(Reply::Opened(3, None))
    ));
    finish_drop.send(()).unwrap();
    assert_eq!(backend_done.recv_timeout(WAIT), Ok(()));
    backend.join().unwrap();
    assert_eq!(
        backend_done.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    );
    assert_eq!(
        starts_drop.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    );
    assert_eq!(plays.load(Ordering::Relaxed), 0);

    next.send(Request::Open(4, Arc::default())).unwrap();
    let Reply::Opened(4, Some(transport)) = results.recv_timeout(WAIT).unwrap() else {
        panic!("the pending backend released its permit");
    };
    next.send(Request::Retire(1)).unwrap();
    assert!(matches!(results.recv_timeout(WAIT), Ok(Reply::Retired(1))));
    next.send(Request::Power(4, Power::Paused)).unwrap();
    assert!(matches!(
        results.recv_timeout(WAIT),
        Ok(Reply::Powered(4, true))
    ));

    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&calls);
    let (refusing, refusals) = spawn_with::<()>(
        thread::current(),
        Instant::now(),
        move |_, _| {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(OutputError::Unavailable)
        },
        |(), _| false,
    )
    .unwrap();
    refusing.send(Request::Open(5, Arc::default())).unwrap();
    assert!(matches!(
        refusals.recv_timeout(WAIT),
        Ok(Reply::Opened(5, None))
    ));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "the old retire did not clear the newer permit"
    );
    drop(transport);
    next.send(Request::Retire(4)).unwrap();
    assert!(matches!(results.recv_timeout(WAIT), Ok(Reply::Retired(4))));
    refusing.send(Request::Open(6, Arc::default())).unwrap();
    assert!(matches!(
        refusals.recv_timeout(WAIT),
        Ok(Reply::Opened(6, None))
    ));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the error was a real backend attempt"
    );

    next.send(Request::Open(7, Arc::default())).unwrap();
    let Reply::Opened(7, Some(transport)) = results.recv_timeout(WAIT).unwrap() else {
        panic!("the backend error released its permit");
    };
    assert_eq!(plays.load(Ordering::Relaxed), 2);
    drop(transport);
    next.send(Request::Retire(7)).unwrap();
    assert!(matches!(results.recv_timeout(WAIT), Ok(Reply::Retired(7))));
    assert!(!NATIVE.load(Ordering::Acquire));
    drop((next, refusing));
    assert!(matches!(
        results.recv_timeout(WAIT),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(matches!(
        refusals.recv_timeout(WAIT),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
}
