use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::Submit;

#[derive(Default)]
pub(super) struct Shared {
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) pending: AtomicU8,
}

pub(super) struct Permit(&'static AtomicBool);

impl Permit {
    pub(super) fn claim(busy: &'static AtomicBool) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self(busy))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) struct Retired {
    pub(super) submit: Submit,
    _permit: Permit,
}

impl Retired {
    pub(super) fn acknowledged(&mut self) -> bool {
        Arc::get_mut(&mut self.submit.shared).is_some()
    }

    pub(super) fn poll_retirement(&mut self) -> bool {
        if self.acknowledged() {
            return true;
        }
        self.submit.clear_available();
        false
    }
}

pub(super) struct Owner {
    retained: Option<Retired>,
    cleanup: SyncSender<Retired>,
}

impl Owner {
    pub(super) fn new(submit: Submit, permit: Permit, cleanup: SyncSender<Retired>) -> Self {
        Self {
            retained: Some(Retired {
                submit,
                _permit: permit,
            }),
            cleanup,
        }
    }

    pub(super) fn submit(&self) -> &Submit {
        &self
            .retained
            .as_ref()
            .expect("live stream has a pool")
            .submit
    }

    pub(super) fn submit_mut(&mut self) -> &mut Submit {
        &mut self
            .retained
            .as_mut()
            .expect("live stream has a pool")
            .submit
    }

    fn cancel(&self) {
        self.submit()
            .shared
            .cancelled
            .store(true, Ordering::Release);
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.cancel();
        let Some(mut retained) = self.retained.take() else {
            return;
        };
        if retained.acknowledged() {
            return;
        }
        retained.submit.clear_available();
        if let Err(TrySendError::Full(retained) | TrySendError::Disconnected(retained)) =
            self.cleanup.try_send(retained)
        {
            // A lost cleanup receiver must not make the callback the final owner. The
            // retained permit limits this failure to one pool and refuses another open.
            std::mem::forget(retained);
        }
    }
}

pub(super) struct OwnedStream<S> {
    pub(super) stream: S,
    pub(super) owner: Owner,
}

impl<S> Drop for OwnedStream<S> {
    fn drop(&mut self) {
        self.owner.cancel();
    }
}

pub(super) fn cleanup_sender() -> Option<SyncSender<Retired>> {
    static CLEANUP: OnceLock<Option<SyncSender<Retired>>> = OnceLock::new();
    CLEANUP
        .get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<Retired>(1);
            std::thread::Builder::new()
                .name("speaker-retirement".into())
                .spawn(move || {
                    while let Ok(mut retained) = receiver.recv() {
                        // Wait for the backend's closures, not its public handle. A stuck
                        // backend retains at most one pool (8 blocks, at most 4 MiB of PCM);
                        // Each poll wipes newly exclusive blocks. Blocks still held by the
                        // callback or a ready slot stay pinned; late callbacks cannot play them.
                        while !retained.poll_retirement() {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                })
                .ok()?;
            Some(sender)
        })
        .clone()
}
