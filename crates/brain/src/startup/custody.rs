//! The session custody the instance item runs under: the private lock leaf in this edition's
//! admitted product folder, held from bootstrap to the item's final delete (A-7, A-10).

use std::fmt;

use crate::platform;

/// Why custody wasn't taken. It carries no path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CustodyError {
    #[error("another brain holds the session")]
    Busy,
    #[error("this edition or platform has no instance item")]
    NotAdmitted,
    #[error("the Application Support folder is unavailable")]
    #[cfg_attr(
        test,
        expect(dead_code, reason = "only a product start resolves the OS folder")
    )]
    Unavailable,
    #[error("the session folder or lock isn't private")]
    Untrusted,
    #[error("the session lock couldn't be opened")]
    Open(std::io::ErrorKind),
}

/// Holds the session lock until dropped; nothing else can make one.
pub(crate) struct SessionCustody {
    _lock: Option<platform::InstanceLock>,
}

impl fmt::Debug for SessionCustody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCustody(..)")
    }
}

impl SessionCustody {
    /// Locks the session leaf in the OS-resolved Support folder's product folder, creating only
    /// that folder when it's missing. Busy refuses, so no item call follows.
    #[cfg_attr(
        test,
        expect(dead_code, reason = "only a product start resolves the OS folder")
    )]
    pub(in crate::startup) fn bootstrap() -> Result<Self, CustodyError> {
        platform::instance_custody().map(Self::held)
    }

    /// `bootstrap` under a test's own Support folder.
    #[cfg(test)]
    pub(in crate::startup) fn bootstrap_at(
        support: &std::path::Path,
    ) -> Result<Self, CustodyError> {
        platform::instance_custody_at(support).map(Self::held)
    }

    fn held(lock: platform::InstanceLock) -> Self {
        Self { _lock: Some(lock) }
    }
}

#[cfg(test)]
pub(in crate::startup) mod tests;
