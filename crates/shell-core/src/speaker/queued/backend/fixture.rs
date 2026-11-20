use super::{
    Arc, AtomicBool, Control, Instant, Receiver, Reply, Request, SyncSender, Thread, io, spawn_with,
};
use crate::platform::speaker_cpal::{OutputError, playback_fixture::FakePlayback};

pub fn serialize_playback() -> std::sync::MutexGuard<'static, ()> {
    static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    EXCLUSIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct FixturePlayback {
    callback: FakePlayback,
    control: Arc<Control>,
    origin: Instant,
}

impl FixturePlayback {
    pub fn ready(&self) -> bool {
        self.callback.ready()
    }

    pub fn render(&mut self, samples: &mut [f32]) {
        self.callback.fill(samples, self.origin.elapsed());
    }

    pub fn stopped(&self) -> bool {
        self.control.stopped()
    }
}

pub(in crate::speaker::queued) fn spawn_fake(
    owner: Thread,
    origin: Instant,
    opened: SyncSender<FixturePlayback>,
) -> io::Result<(SyncSender<Request>, Receiver<Reply>)> {
    static OCCUPIED: AtomicBool = AtomicBool::new(false);
    spawn_with(
        owner,
        origin,
        move |control, origin| {
            let (transport, callback) = FakePlayback::try_open(Arc::clone(control), &OCCUPIED)
                .ok_or(OutputError::Unavailable)?;
            opened
                .try_send(FixturePlayback {
                    callback,
                    control: Arc::clone(control),
                    origin,
                })
                .map_err(|_| OutputError::Unavailable)?;
            Ok(((), transport))
        },
        |(), _| true,
    )
}
