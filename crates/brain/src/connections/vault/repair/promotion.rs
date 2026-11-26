use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};

use crate::connections::error::VaultError;
use crate::connections::vault::Vault;
use crate::platform::credential::CredentialStore;
use crate::store::{Boundary, StoreError};

/// What a sign-in's history may be read as: the whole of it, only up to its recovered cutoff,
/// or none of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Proof {
    Ordinary,
    Recovered(Boundary),
    Unproved,
}

type Queue = Arc<dyn Fn(CutoffRequest) -> bool + Send + Sync>;

/// Where a [`HistoryLink`] queues: nowhere yet, the writer, or nowhere ever again.
enum Writer {
    Waiting,
    Bound(Queue),
    Closed,
}

/// The vault's binding to the profile's one history writer: the store thread's store, which
/// ordinary startup opens and purge guards. The writer serves each request as one of its
/// operations. It is bound at most once, to that writer only, and once released never again;
/// while it isn't bound, every request is refused.
#[derive(Clone)]
pub(crate) struct HistoryLink(Arc<Mutex<Writer>>);

/// The brain's side of a link from [`HistoryLink::pending`]: it binds the link once the store
/// thread runs, and releases it before the store closes, because that thread ends only once
/// nothing can queue for it. Dropping it releases too.
pub(crate) struct HistoryBinding(Arc<Mutex<Writer>>);

impl HistoryLink {
    /// `send` queues a request for the writer; false when the writer has gone.
    pub(crate) fn new(send: impl Fn(CutoffRequest) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(Mutex::new(Writer::Bound(Arc::new(send)))))
    }

    pub(crate) fn unbound() -> Self {
        Self(Arc::new(Mutex::new(Writer::Closed)))
    }

    /// A link no writer serves yet, and the binding that gives it one.
    pub(crate) fn pending() -> (Self, HistoryBinding) {
        let writer = Arc::new(Mutex::new(Writer::Waiting));
        (Self(Arc::clone(&writer)), HistoryBinding(writer))
    }

    /// The committed cutoff for `tag`, read again by the writer.
    pub(crate) fn check(&self, tag: u64) -> Result<Boundary, StoreError> {
        let (reply, answer) = mpsc::sync_channel(1);
        let queued = match &*lock(&self.0) {
            Writer::Bound(queue) => queue(CutoffRequest { tag, reply }),
            Writer::Waiting | Writer::Closed => false,
        };
        if !queued {
            return Err(StoreError::RecoveryContext);
        }
        answer.recv().unwrap_or(Err(StoreError::RecoveryContext))
    }
}

impl HistoryBinding {
    /// Binds the link to `writer`'s queue, when the link still waits and `writer` is bound.
    pub(crate) fn bind(&self, writer: &HistoryLink) {
        let queue = match &*lock(&writer.0) {
            Writer::Bound(queue) => Arc::clone(queue),
            Writer::Waiting | Writer::Closed => return,
        };
        let mut link = lock(&self.0);
        if matches!(*link, Writer::Waiting) {
            *link = Writer::Bound(queue);
        }
    }

    /// Ends the link for good. What it already queued is still served; nothing more queues.
    pub(crate) fn release(&self) {
        *lock(&self.0) = Writer::Closed;
    }
}

/// A binding with no link, for an account system whose vault has none.
impl Default for HistoryBinding {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Writer::Closed)))
    }
}

impl Drop for HistoryBinding {
    fn drop(&mut self) {
        self.release();
    }
}

impl fmt::Debug for HistoryBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HistoryBinding")
    }
}

/// Every write to a `Writer` is a single assignment, so even a poisoned lock holds a whole one.
fn lock(writer: &Mutex<Writer>) -> MutexGuard<'_, Writer> {
    writer.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One request for the history writer: read `tag`'s committed cutoff again.
pub(crate) struct CutoffRequest {
    tag: u64,
    reply: mpsc::SyncSender<Result<Boundary, StoreError>>,
}

impl CutoffRequest {
    /// Runs the read for the request's tag and answers it.
    pub(crate) fn serve(self, read: impl FnOnce(u64) -> Result<Boundary, StoreError>) {
        self.reply.send(read(self.tag)).ok();
    }
}

impl<S: CredentialStore> Vault<S> {
    /// The established replacement's cutoff, once the history writer reads its row again and
    /// it matches; every read for the replacement keeps to it. A missing, changed or unreadable
    /// row leaves the replacement locked.
    pub(crate) fn established_repair_context(&self) -> Result<Boundary, VaultError> {
        let (tag, bounds) = self.authority.established_repair_context()?;
        match self.history.check(tag) {
            Ok(boundary) if boundary.bounds() == bounds => Ok(boundary),
            _ => Err(VaultError::Fenced),
        }
    }

    /// What `connection`'s sign-in is: ordinary only at the ordinary schema, recovered only as
    /// the established replacement under exactly that connection while it isn't signing out,
    /// and unproved otherwise, a repaired account no row explains included.
    pub(crate) fn proof(&self, connection: u64) -> Proof {
        match self.authority.ordinary_account() {
            Ok(true) => return Proof::Ordinary,
            Ok(false) => {}
            Err(_) => return Proof::Unproved,
        }
        match (
            self.authority.recovered_signing_out(),
            self.established_repair_context(),
        ) {
            (Ok(false), Ok(cut)) if cut.tag() == connection => Proof::Recovered(cut),
            _ => Proof::Unproved,
        }
    }
}
