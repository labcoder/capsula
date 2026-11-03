//! The root certificates the account client trusts, read from the operating system's store.

use std::fmt;

/// Root certificates as owned public DER, plus how many the store held that weren't kept.
#[derive(Clone, PartialEq, Eq)]
pub struct TrustRoots {
    /// Each kept root's DER encoding.
    pub certificates: Vec<Vec<u8>>,
    /// Roots left out: not valid now, not for server authentication, distrusted by Windows,
    /// unreadable, oversized or past the cap.
    pub skipped: usize,
    /// The skipped roots whose validity, key usage or distrust data couldn't be read; a healthy
    /// store has none.
    pub unreadable: usize,
}

impl fmt::Debug for TrustRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustRoots")
            .field("certificates", &self.certificates.len())
            .field("skipped", &self.skipped)
            .field("unreadable", &self.unreadable)
            .finish()
    }
}

/// The roots couldn't be loaded. Carries OS error codes only, never certificate data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrustRootsError {
    /// The OS refused to open the ROOT or Disallowed store with this error code.
    #[error("root store open failed ({0})")]
    Open(i32),
    /// Walking the store stopped early with this error code.
    #[error("root store read failed ({0})")]
    Enumerate(i32),
    /// No root was usable, so no server could be verified.
    #[error("no usable root certificate")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::TrustRoots;

    #[test]
    fn debug_shows_counts_only() {
        let roots = TrustRoots {
            certificates: vec![vec![0x30, 0x03, 0x02, 0x01, 0x07]; 2],
            skipped: 3,
            unreadable: 1,
        };
        assert_eq!(
            format!("{roots:?}"),
            "TrustRoots { certificates: 2, skipped: 3, unreadable: 1 }"
        );
    }
}
