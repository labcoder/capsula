use super::{
    Arc, AtomicBool, Control, Instant, NATIVE, Ordering, Receiver, Reply, Request, SyncSender,
    Thread, io, spawn_with, thread,
};
use crate::platform::speaker_cpal::{OutputError, playback_fixture::FakePlayback};
use std::time::Duration;

static OCCUPIED: AtomicBool = AtomicBool::new(false);

/// Serializes fake sessions through disposal of the previous session's callback and device.
///
/// # Panics
/// Panics if the previous fixture left a device or playback pool live past the test deadline.
pub fn serialize_playback() -> std::sync::MutexGuard<'static, ()> {
    static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let permit = EXCLUSIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    await_fake_disposal();
    permit
}

/// Waits for a previous route under the fixture's existing serialization guard.
pub fn await_fake_disposal() {
    wait_for_playback_disposal(&[&NATIVE, &OCCUPIED]);
}

pub(super) fn wait_for_playback_disposal(permits: &[&AtomicBool]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while permits.iter().any(|permit| permit.load(Ordering::Acquire)) {
        assert!(
            Instant::now() < deadline,
            "the previous playback fixture returns its permits"
        );
        thread::sleep(Duration::from_millis(2));
    }
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

    /// Exclusive source position handed to this callback, not a heard receipt.
    pub fn handed(&self) -> u64 {
        self.callback.handed()
    }

    /// The contiguous source prefix the worker settled on this original physical route.
    #[cfg(all(windows, debug_assertions))]
    #[cfg(any(test, feature = "test-support"))]
    pub fn presented_prefix(&self) -> u64 {
        self.control.presented_prefix()
    }

    pub fn stopped(&self) -> bool {
        self.control.stopped()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{Control, FixturePlayback};

    impl FixturePlayback {
        pub(crate) fn render_during_copy(
            &mut self,
            samples: &mut [f32],
            action: impl FnOnce(&Control),
        ) {
            self.callback
                .fill_during_copy(samples, self.origin.elapsed(), action);
        }
    }
}

pub(in crate::speaker::queued) fn spawn_fake(
    owner: Thread,
    origin: Instant,
    opened: SyncSender<FixturePlayback>,
) -> io::Result<(SyncSender<Request>, Receiver<Reply>)> {
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
