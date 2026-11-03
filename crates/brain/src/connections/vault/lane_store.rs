//! The account lane's one handle on the credential store. Rez's store has answered "not found"
//! after a successful write and kept a name after a successful delete, so the lane takes
//! neither at its word.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use crate::platform::credential::{CredentialStore, CredentialStoreError, SecretBytes};

/// How often a read or delete the store contradicts is tried [guess].
pub(super) const TRIES: usize = 4;
/// The wait between those tries [guess].
const WAIT: Duration = Duration::from_millis(25);

/// Wraps the store a vault owns; only the account lane's worker holds a vault outside tests.
pub(super) struct LaneStore<S> {
    store: S,
    /// Names this handle wrote and hasn't deleted since: "not found" for one is a miss.
    written: Mutex<BTreeSet<String>>,
}

impl<S: CredentialStore> LaneStore<S> {
    pub(super) fn new(store: S) -> Self {
        Self {
            store,
            written: Mutex::default(),
        }
    }

    fn written(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.written.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<S: CredentialStore> CredentialStore for LaneStore<S> {
    fn get(&self, target: &str) -> Result<Option<SecretBytes>, CredentialStoreError> {
        if !self.written().contains(target) {
            return self.store.get(target);
        }
        for attempt in 0..TRIES {
            if attempt > 0 {
                thread::sleep(WAIT);
            }
            if let Some(value) = self.store.get(target)? {
                return Ok(Some(value));
            }
        }
        Err(CredentialStoreError::Unsettled)
    }

    fn set(&self, target: &str, secret: &SecretBytes) -> Result<(), CredentialStoreError> {
        self.store.set(target, secret)?;
        self.written().insert(target.to_owned());
        Ok(())
    }

    fn delete(&self, target: &str) -> Result<(), CredentialStoreError> {
        for attempt in 0..TRIES {
            if attempt > 0 {
                thread::sleep(WAIT);
            }
            self.store.delete(target)?;
            if self.store.get(target)?.is_none() {
                self.written().remove(target);
                return Ok(());
            }
        }
        Err(CredentialStoreError::Unsettled)
    }
}
