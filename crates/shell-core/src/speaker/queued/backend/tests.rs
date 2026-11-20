use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use crate::platform::speaker_cpal::tests::FakePlayback;

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
