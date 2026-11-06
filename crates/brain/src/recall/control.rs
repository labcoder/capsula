//! One read's cancellation and absolute deadline; a scoped SQLite handler owns its interrupt.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::Error;

pub(crate) const READ_TIME: Duration = Duration::from_secs(2);
pub(crate) const POLL: Duration = Duration::from_millis(5);

impl From<crate::store::StoreError> for Error {
    fn from(_: crate::store::StoreError) -> Self {
        Self::Read
    }
}

impl From<Error> for crate::store::StoreError {
    fn from(error: Error) -> Self {
        match error {
            Error::Timeout => Self::RecallBudget,
            Error::Ended => Self::Closed,
            _ => Self::Read,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Control {
    #[cfg(test)]
    pub(crate) test: Arc<tests::State>,
    pub(crate) end: Instant,
    cancel: Arc<AtomicBool>,
    parents: Vec<Arc<AtomicBool>>,
}

impl Control {
    pub(crate) fn new(end: Instant) -> Self {
        Self {
            #[cfg(test)]
            test: Arc::new(tests::State::default()),
            end,
            cancel: Arc::new(AtomicBool::new(false)),
            parents: Vec::new(),
        }
    }

    pub(crate) fn until(&self, end: Instant) -> Self {
        Self {
            end: self.end.min(end),
            ..self.clone()
        }
    }

    pub(crate) fn observe(&self, flag: Arc<AtomicBool>) -> Self {
        let mut next = self.clone();
        next.parents.push(flag);
        next
    }

    pub(crate) fn child(&self) -> Self {
        let mut next = Self::new(self.end);
        next.parents.clone_from(&self.parents);
        next.parents.push(Arc::clone(&self.cancel));
        next
    }

    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::SeqCst)
            || self.parents.iter().any(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(Error::Ended);
        }
        #[cfg(test)]
        let now = self.test.now();
        #[cfg(not(test))]
        let now = Instant::now();
        if now >= self.end {
            return Err(Error::Timeout);
        }
        Ok(())
    }

    pub(crate) fn with_handler<T>(
        &self,
        conn: &mut Connection,
        read: impl FnOnce(&Connection, &ReadScope) -> T,
    ) -> Result<T, Error> {
        let scope = ReadScope {
            control: self.clone(),
            active: Arc::new(AtomicBool::new(false)),
            fired: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let handler = scope.clone();
        let installed = conn.progress_handler(
            1000,
            Some(move || {
                #[cfg(test)]
                if handler.control.test.progress() {
                    return true;
                }
                let stop =
                    handler.active.load(Ordering::SeqCst) && handler.control.check().is_err();
                if stop {
                    handler.fired.store(true, Ordering::SeqCst);
                }
                stop
            }),
        );
        #[cfg(test)]
        let installed = self.test.hook(installed, 1);
        installed.map_err(|_| Error::Handler)?;
        let mut reset = Reset(
            Some(conn),
            #[cfg(test)]
            self.clone(),
        );
        let result = read(
            reset.0.as_deref().expect("the handler owns its connection"),
            &scope,
        );
        reset.finish()?;
        if scope.failed.load(Ordering::SeqCst) {
            return Err(Error::Handler);
        }
        Ok(result)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReadScope {
    control: Control,
    active: Arc<AtomicBool>,
    fired: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl ReadScope {
    pub(crate) fn query<T>(
        &self,
        conn: &Connection,
        read: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.control.check()?;
        let busy: u64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
            .map_err(|_| Error::Read)?
            .try_into()
            .map_err(|_| Error::Read)?;
        conn.busy_timeout(Duration::ZERO).map_err(|_| Error::Read)?;
        let mut reset = Active(self, Some(conn), Duration::from_millis(busy));
        self.fired.store(false, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
        let result = read();
        let result = if result.is_ok() || self.fired.load(Ordering::SeqCst) {
            self.control.check().and(result)
        } else {
            result
        };
        reset.finish()?;
        result
    }
}

#[derive(Debug)]
pub(crate) struct CancelOnDrop(pub(crate) Control);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Reset<'a>(Option<&'a mut Connection>, #[cfg(test)] Control);
impl Reset<'_> {
    fn finish(&mut self) -> Result<(), Error> {
        if let Some(conn) = self.0.take() {
            let removed = conn.progress_handler(0, None::<fn() -> bool>);
            #[cfg(test)]
            let removed = self.1.test.hook(removed, 2);
            removed.map_err(|_| Error::Handler)?;
        }
        Ok(())
    }
}
impl Drop for Reset<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::error!(%error, "history cleanup failed during unwind");
        }
    }
}

struct Active<'a>(&'a ReadScope, Option<&'a Connection>, Duration);
impl Active<'_> {
    fn finish(&mut self) -> Result<(), Error> {
        self.0.active.store(false, Ordering::SeqCst);
        if let Some(conn) = self.1.take() {
            let restored = conn.busy_timeout(self.2);
            #[cfg(test)]
            let restored = self.0.control.test.hook(restored, 3);
            if restored.is_err() {
                self.0.failed.store(true, Ordering::SeqCst);
                return Err(Error::Handler);
            }
        }
        Ok(())
    }
}
impl Drop for Active<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::error!(%error, "history timeout cleanup failed during unwind");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
