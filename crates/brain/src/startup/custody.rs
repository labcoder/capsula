//! The session custody the instance item runs under: the private lock leaf in this edition's
//! admitted product folder, held from bootstrap to the item's final delete (A-7, A-10).

use std::fmt;
use std::sync::Arc;

use crate::platform::{self, SessionLock};

pub(crate) use crate::platform::CustodyError;

/// Holds the session lock until dropped; nothing else can make one, or lend it.
pub(crate) struct SessionCustody {
    /// The held lock with the product path it was taken in; loans share it.
    held: Option<Arc<platform::InstanceLock>>,
    #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
    _fixture: Option<Arc<()>>,
}

impl fmt::Debug for SessionCustody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCustody(..)")
    }
}

impl SessionCustody {
    /// Locks the session leaf in the OS-resolved Support folder's product folder, creating only
    /// that folder when it's missing. Busy refuses, so no item call follows.
    pub(in crate::startup) fn bootstrap() -> Result<Self, CustodyError> {
        platform::instance_custody().map(Self::held)
    }

    /// `bootstrap` under a test's own Support folder.
    #[cfg(test)]
    pub(crate) fn bootstrap_at(support: &std::path::Path) -> Result<Self, CustodyError> {
        platform::instance_custody_at(support).map(Self::held)
    }

    /// Custody that holds a token instead of a lock, so offline tests can watch when it drops.
    #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
    pub(crate) fn fixture(token: Arc<()>) -> Self {
        Self {
            held: None,
            _fixture: Some(token),
        }
    }

    /// The held lock as the opened state's profile lock, bound to the product path it was
    /// taken in; the lock releases only with custody and the last loan. A token lends nothing.
    pub(in crate::startup) fn lend(&self) -> Option<Arc<SessionLock>> {
        let lock = self.held.as_ref()?;
        Some(Arc::new(SessionLock::lent(Arc::clone(lock))))
    }

    fn held(lock: platform::InstanceLock) -> Self {
        Self {
            held: Some(Arc::new(lock)),
            #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
            _fixture: None,
        }
    }
}
