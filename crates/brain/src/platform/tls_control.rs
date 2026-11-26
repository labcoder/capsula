use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Notify, watch};
use tokio::time::{Instant, error::Elapsed, timeout_at};

#[derive(Debug, Default)]
pub(crate) struct ConnectCut {
    gate: Mutex<()>,
    closed: AtomicBool,
    changed: Notify,
}

impl ConnectCut {
    pub(crate) fn close(&self) {
        self.close_with(|| ());
    }

    pub(crate) fn close_with<T>(&self, seal: impl FnOnce() -> T) -> T {
        let _gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let result = seal();
        self.closed.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
        result
    }

    pub(crate) fn with<T>(&self, poll: impl FnOnce() -> T) -> io::Result<T> {
        let _gate = self.gate.lock().map_err(|_| io::ErrorKind::Interrupted)?;
        if self.closed() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        Ok(poll())
    }

    pub(super) fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.gate.is_poisoned()
    }

    pub(crate) async fn until_closed(&self) {
        let changed = self.changed.notified();
        if !self.closed() {
            changed.await;
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn connect_scope<T>(
    by: Instant,
    cancel: Option<&watch::Receiver<bool>>,
    work: impl Future<Output = T>,
) -> impl Future<Output = T> {
    scope(Some(by), cancel, None, work)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn connect_scope<F>(_: Instant, _: Option<&watch::Receiver<bool>>, work: F) -> F {
    work
}

pub(crate) fn connect_timeout_at<T>(
    by: Instant,
    work: impl Future<Output = T>,
) -> impl Future<Output = Result<T, Elapsed>> {
    connect_scope(by, None, timeout_at(by, work))
}

#[cfg(target_os = "macos")]
pub(crate) fn connect_owned_scope<T, F: Future<Output = T>>(
    by: Option<Instant>,
    cut: &Arc<ConnectCut>,
    work: F,
) -> impl Future<Output = T> + use<T, F> {
    scope(by, None, Some(cut), work)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn connect_owned_scope<F>(_: Option<Instant>, _: &Arc<ConnectCut>, work: F) -> F {
    work
}

#[cfg(target_os = "macos")]
fn scope<T, F: Future<Output = T>>(
    by: Option<Instant>,
    cancel: Option<&watch::Receiver<bool>>,
    cut: Option<&Arc<ConnectCut>>,
    work: F,
) -> impl Future<Output = T> + use<T, F> {
    let mut control = CURRENT.try_with(Clone::clone).unwrap_or_default();
    control.restrict(by, cancel, cut);
    CURRENT.scope(control, work)
}

#[cfg(target_os = "macos")]
pub(crate) async fn connect_with_cut<T>(
    cut: Arc<ConnectCut>,
    work: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    if let Ok(mut control) = CURRENT.try_with(Clone::clone) {
        control.add_cut(cut)?;
        return CURRENT.scope(control, work).await;
    }
    work.await
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn connect_with_cut<F>(_: Arc<ConnectCut>, work: F) -> F {
    work
}

#[cfg(target_os = "macos")]
use mac::CURRENT;
#[cfg(target_os = "macos")]
pub(super) use mac::Control;

#[cfg(target_os = "macos")]
mod mac {
    use super::{Arc, ConnectCut, Instant, io, watch};

    #[derive(Clone, Default)]
    pub(in crate::platform) struct Control {
        by: Option<std::time::Instant>,
        cuts: [Option<Arc<ConnectCut>>; 2],
        download: Option<watch::Receiver<bool>>,
        invalid: bool,
    }

    tokio::task_local! {
        pub(super) static CURRENT: Control;
    }

    impl Control {
        pub(super) fn restrict(
            &mut self,
            by: Option<Instant>,
            cancel: Option<&watch::Receiver<bool>>,
            cut: Option<&Arc<ConnectCut>>,
        ) {
            if let Some(by) = by.map(Instant::into_std) {
                self.by = Some(self.by.map_or(by, |outer| outer.min(by)));
            }
            if let Some(cancel) = cancel {
                if let Some(outer) = &self.download {
                    self.invalid |= !outer.same_channel(cancel);
                } else {
                    self.download = Some(cancel.clone());
                }
            }
            if let Some(cut) = cut {
                self.invalid |= self.add_cut(Arc::clone(cut)).is_err();
            }
        }

        pub(super) fn add_cut(&mut self, cut: Arc<ConnectCut>) -> io::Result<()> {
            if self.cuts.iter().flatten().any(|on| Arc::ptr_eq(on, &cut)) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let free = self
                .cuts
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(io::ErrorKind::Interrupted)?;
            *free = Some(cut);
            Ok(())
        }

        pub(in crate::platform) fn current() -> io::Result<Self> {
            CURRENT
                .try_with(Clone::clone)
                .ok()
                .filter(|control| control.by.is_some() && !control.invalid)
                .ok_or_else(|| io::ErrorKind::PermissionDenied.into())
        }

        #[cfg(test)]
        pub(in crate::platform) fn fixture(by: std::time::Instant, cut: Arc<ConnectCut>) -> Self {
            Self {
                by: Some(by),
                cuts: [Some(cut), None],
                ..Self::default()
            }
        }

        pub(in crate::platform) fn deadline(
            &self,
            connect_cap: std::time::Duration,
        ) -> std::time::Instant {
            let cap = std::time::Instant::now() + connect_cap;
            self.by.map_or(cap, |by| by.min(cap))
        }

        pub(in crate::platform) fn cancelled(&self) -> bool {
            self.cuts.iter().flatten().any(|cut| cut.closed())
                || self
                    .download
                    .as_ref()
                    .is_some_and(|cancel| *cancel.borrow())
        }

        pub(in crate::platform) async fn until_cancelled(&self) {
            async fn cut_closed(cut: Option<&ConnectCut>) {
                if let Some(cut) = cut {
                    cut.until_closed().await;
                } else {
                    std::future::pending::<()>().await;
                }
            }
            tokio::select! {
                () = cut_closed(self.cuts[0].as_deref()) => {},
                () = cut_closed(self.cuts[1].as_deref()) => {},
                () = async {
                    if let Some(mut cancel) = self.download.clone()
                        && cancel.wait_for(|closed| *closed).await.is_ok()
                    {
                        return;
                    }
                    std::future::pending::<()>().await;
                } => {},
            }
        }

        pub(in crate::platform) fn with<T>(
            &self,
            poll: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<T> {
            fn with_cut<T>(
                cut: Option<&ConnectCut>,
                poll: impl FnOnce() -> io::Result<T>,
            ) -> io::Result<T> {
                match cut {
                    Some(cut) => cut.with(poll)?,
                    None => poll(),
                }
            }
            with_cut(self.cuts[0].as_deref(), || {
                with_cut(self.cuts[1].as_deref(), || {
                    let download = self.download.as_ref().map(watch::Receiver::borrow);
                    if download.as_ref().is_some_and(|closed| **closed) {
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                    poll()
                })
            })
        }
    }
}
