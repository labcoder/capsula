use std::cell::Cell;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use tokio::sync::{mpsc, watch};

use crate::platform::shell_process::Verdict;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fatal {
    Refused,
    ParentExited,
    ParentUnconfirmed,
}

#[derive(Clone)]
pub(crate) struct Terminal {
    pair: watch::Receiver<bool>,
    parent: Option<watch::Receiver<Verdict>>,
}

#[derive(Clone, Copy)]
enum Read {
    Initial,
    Changed,
}

impl Terminal {
    pub(crate) fn new(
        pair: watch::Receiver<bool>,
        parent: Option<watch::Receiver<Verdict>>,
    ) -> Self {
        Self { pair, parent }
    }

    /// The fatal cause already set, read as it stands rather than as a change.
    pub(crate) fn fatal(&self) -> Option<Fatal> {
        self.cause(Read::Initial)
    }

    fn cause(&self, read: Read) -> Option<Fatal> {
        if let Some(parent) = &self.parent {
            let changed = parent.has_changed();
            if matches!(read, Read::Initial) || !matches!(changed, Ok(false)) {
                match *parent.borrow() {
                    Verdict::Exited => return Some(Fatal::ParentExited),
                    Verdict::Unconfirmed(_) => return Some(Fatal::ParentUnconfirmed),
                    Verdict::Watching if changed.is_err() => {
                        return Some(Fatal::ParentUnconfirmed);
                    }
                    Verdict::Watching => {}
                }
            }
        }
        let changed = self.pair.has_changed();
        (changed.is_err()
            || ((matches!(read, Read::Initial) || !matches!(changed, Ok(false)))
                && *self.pair.borrow()))
        .then_some(Fatal::Refused)
    }

    pub(super) async fn wake_on_fatal(mut self) {
        let (notify, mut notified) = mpsc::channel(1);
        let waker = Waker::from(Arc::new(IdleWake(notify)));
        let mut change = pin!(async {
            if let Some(parent) = &mut self.parent {
                tokio::select! {
                    _ = parent.wait_for(|verdict| *verdict != Verdict::Watching) => {}
                    _ = self.pair.wait_for(|refused| *refused) => {}
                }
            } else {
                drop(self.pair.wait_for(|refused| *refused).await);
            }
        });
        while change
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
        {
            notified
                .recv()
                .await
                .expect("the watch waker owns the sender");
        }
    }
}

struct IdleWake(mpsc::Sender<()>);

impl Wake for IdleWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let _coalesced_or_closed = self.0.try_send(());
    }
}

#[derive(Clone, Copy)]
enum Disposition {
    Fatal(Fatal),
    Quit,
}

pub(super) struct Fence {
    terminal: Option<Terminal>,
    first: Cell<Option<Disposition>>,
}

impl Fence {
    pub(super) fn new(terminal: Option<Terminal>) -> Self {
        let first = terminal
            .as_ref()
            .and_then(Terminal::fatal)
            .map(Disposition::Fatal);
        Self {
            terminal,
            first: Cell::new(first),
        }
    }

    pub(super) fn check(&self) -> Option<Fatal> {
        match self.first.get() {
            Some(Disposition::Fatal(cause)) => Some(cause),
            Some(Disposition::Quit) => None,
            None => {
                let cause = self.terminal.as_ref()?.cause(Read::Changed)?;
                self.first.set(Some(Disposition::Fatal(cause)));
                Some(cause)
            }
        }
    }

    pub(super) fn accept_quit(&self) -> Result<(), Fatal> {
        if let Some(cause) = self.check() {
            return Err(cause);
        }
        self.first.set(Some(Disposition::Quit));
        Ok(())
    }

    pub(super) fn poll_fatal(
        &self,
        wake: Pin<&mut impl Future<Output = ()>>,
        cx: &mut Context<'_>,
    ) -> Poll<Fatal> {
        if let Some(cause) = self.check() {
            return Poll::Ready(cause);
        }
        if matches!(self.first.get(), Some(Disposition::Quit)) || wake.poll(cx).is_pending() {
            return Poll::Pending;
        }
        Poll::Ready(
            self.check()
                .expect("the idle wake observed a terminal cell"),
        )
    }
}
