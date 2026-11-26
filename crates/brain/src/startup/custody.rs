//! The session custody the instance item runs under: the private lock leaf in this edition's
//! admitted product folder, held from bootstrap to the item's final delete (A-7, A-10).

use std::fmt;

use crate::platform;

pub(crate) use crate::platform::CustodyError;

/// Holds the session lock until dropped; nothing else can make one.
pub(crate) struct SessionCustody {
    _lock: Option<platform::InstanceLock>,
    #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
    _fixture: Option<std::sync::Arc<()>>,
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
    pub(crate) fn fixture(token: std::sync::Arc<()>) -> Self {
        Self {
            _lock: None,
            _fixture: Some(token),
        }
    }

    fn held(lock: platform::InstanceLock) -> Self {
        Self {
            _lock: Some(lock),
            #[cfg(any(test, all(debug_assertions, feature = "test-support")))]
            _fixture: None,
        }
    }
}
