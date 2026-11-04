use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{CredentialStore, NativeCredentialStore};

/// Serial native fixture access; `docs/development/accounts.md` says why the store needs it.
static STORE: Mutex<()> = Mutex::new(());

#[must_use]
pub(crate) struct Fixture {
    targets: Vec<String>,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    pub(crate) fn new(targets: Vec<String>) -> Self {
        Self {
            targets,
            _guard: STORE.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let store = NativeCredentialStore;
        let mut failed = false;
        for target in &self.targets {
            if let Err(error) = store.delete(target) {
                tracing::error!(%error, "real credential fixture cleanup delete failed");
                failed = true;
            }
        }
        for target in &self.targets {
            match store.get(target) {
                Ok(None) => {}
                Ok(Some(_)) => {
                    tracing::error!("real credential fixture cleanup left an owned name");
                    failed = true;
                }
                Err(error) => {
                    tracing::error!(%error, "real credential fixture cleanup read failed");
                    failed = true;
                }
            }
        }
        assert!(
            !failed || std::thread::panicking(),
            "real credential fixture cleanup must remove every owned name"
        );
    }
}
