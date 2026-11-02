//! The store's side connections: the page reader's and the checkpoint helper's. Each sits in its
//! own slot and opens lazily, never creating the file. A swap holds both slots from before its
//! checkpoint until the serving connection is back, so neither can open the old file meanwhile.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::StoreError;

type Slot = Mutex<Option<Connection>>;

/// The side connections, shared by the store's thread, the page reader and the helper.
#[derive(Debug)]
pub struct Side {
    path: PathBuf,
    reader: Slot,
    helper: Slot,
    /// Whether the serving connection has the file open; written only while both slots are held.
    available: AtomicBool,
    /// A clean copy waits to be swapped in, so neither slot starts new work.
    swapping: Mutex<bool>,
    swapped: Condvar,
}

/// Both slots, their connections closed. Held across a swap or a reconnect.
pub(super) struct Held<'a> {
    side: &'a Side,
    _reader: MutexGuard<'a, Option<Connection>>,
    _helper: MutexGuard<'a, Option<Connection>>,
}

impl Held<'_> {
    /// The serving connection has the file open, so the side connections may open it too.
    pub(super) fn available(&self) {
        self.side.available.store(true, Ordering::SeqCst);
    }

    /// The file is closed, and the side connections mustn't open it.
    pub(super) fn unavailable(&self) {
        self.side.available.store(false, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

impl Side {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            reader: Mutex::new(None),
            helper: Mutex::new(None),
            available: AtomicBool::new(true),
            swapping: Mutex::new(false),
            swapped: Condvar::new(),
        }
    }

    /// Takes both slots without waiting, the reader's first, and closes their connections;
    /// `None`, closing nothing, while either is in use.
    pub(super) fn try_hold(&self) -> Option<Held<'_>> {
        let mut reader = try_lock(&self.reader)?;
        let mut helper = try_lock(&self.helper)?;
        (*reader, *helper) = (None, None);
        Some(Held {
            side: self,
            _reader: reader,
            _helper: helper,
        })
    }

    /// Takes both slots, the reader's first, and closes their connections.
    pub(super) fn hold(&self) -> Held<'_> {
        let mut reader = lock(&self.reader);
        let mut helper = lock(&self.helper);
        (*reader, *helper) = (None, None);
        Held {
            side: self,
            _reader: reader,
            _helper: helper,
        }
    }

    /// Takes the helper's connection, waiting while a checkpoint runs on it.
    pub(crate) fn hold_helper(&self) -> MutexGuard<'_, Option<Connection>> {
        self.hold_helper_after(|| ())
    }

    /// [`Side::hold_helper`], calling `waiting` first when it has to wait.
    pub(crate) fn hold_helper_after(
        &self,
        waiting: impl FnOnce(),
    ) -> MutexGuard<'_, Option<Connection>> {
        try_lock(&self.helper).unwrap_or_else(|| {
            waiting();
            lock(&self.helper)
        })
    }

    /// Closes both connections for good, as the store's thread ends; a read waiting on a swap
    /// then fails.
    pub(crate) fn close(&self) {
        self.hold().unavailable();
        self.end_swap();
    }

    /// From here until [`Side::end_swap`], neither slot starts new work.
    pub(crate) fn begin_swap(&self) {
        *lock(&self.swapping) = true;
    }

    pub(crate) fn end_swap(&self) {
        *lock(&self.swapping) = false;
        self.swapped.notify_all();
    }

    /// Runs `read` on the reader's connection, opening it first if needed. Waits while a swap
    /// is ready, holding no slot.
    ///
    /// # Errors
    /// Fails when the store is unavailable, its file can't be opened, or `read` fails.
    pub(crate) fn read<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        loop {
            let mut slot = lock(&self.reader);
            if !*lock(&self.swapping) {
                return read(self.open(&mut slot, Self::reader)?);
            }
            drop(slot);
            drop(
                self.swapped
                    .wait_while(lock(&self.swapping), |swapping| *swapping)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }

    /// Copies what it can of the WAL into the main file, waiting for no one; false when some is
    /// left, because a reader still needs it, another checkpoint ran, or a swap is ready.
    ///
    /// # Errors
    /// Fails when the store is unavailable, its file can't be opened, or SQLite fails.
    pub(crate) fn checkpoint(&self) -> Result<bool, StoreError> {
        self.checkpoint_held(self.hold_helper())
    }

    /// [`Side::checkpoint`] on a helper's connection already taken with [`Side::hold_helper`].
    ///
    /// # Errors
    /// As [`Side::checkpoint`].
    pub(crate) fn checkpoint_held(
        &self,
        mut slot: MutexGuard<'_, Option<Connection>>,
    ) -> Result<bool, StoreError> {
        if *lock(&self.swapping) {
            return Ok(false);
        }
        let (busy, log, copied) = self.open(&mut slot, Self::helper)?.query_row(
            "PRAGMA wal_checkpoint(PASSIVE)",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        Ok(busy == 0 && copied == log)
    }

    fn open<'a>(
        &self,
        slot: &'a mut Option<Connection>,
        connect: fn(&Self) -> Result<Connection, StoreError>,
    ) -> Result<&'a Connection, StoreError> {
        if slot.is_none() {
            if !self.available.load(Ordering::SeqCst) {
                return Err(StoreError::Closed);
            }
            *slot = Some(connect(self)?);
        }
        Ok(slot.as_ref().expect("the slot was just filled"))
    }

    /// Read-only and query-only, with the serving connection's in-memory temp store.
    fn reader(&self) -> Result<Connection, StoreError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA query_only = 1; PRAGMA temp_store = MEMORY;")?;
        Ok(conn)
    }

    /// Read-write, since a checkpoint writes the main file, and never waiting on a lock.
    fn helper(&self) -> Result<Connection, StoreError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::ZERO)?;
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        Ok(conn)
    }
}

#[cfg(test)]
mod tests;
