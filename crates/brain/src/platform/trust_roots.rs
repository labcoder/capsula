//! The root certificates the account client trusts, read from the operating system's store.

use std::fmt;
use std::sync::Arc;

use rustls::pki_types::CertificateDer;

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
    /// This build has no native trust-store adapter.
    #[error("native trust roots are unsupported on this platform")]
    Unsupported,
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

pub(crate) const fn native_trust_supported() -> bool {
    cfg!(windows)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeTlsError {
    #[error("native TLS is unsupported on this platform")]
    Unsupported,
    #[error("native TLS trust store unavailable")]
    TrustStoreUnavailable,
    #[error("native TLS configuration failed")]
    Configuration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustDiagnostics {
    Account,
    Quiet,
}

pub(crate) fn native_client_tls(
    diagnostics: TrustDiagnostics,
) -> Result<rustls::ClientConfig, NativeTlsError> {
    client_tls_from_roots(super::trust_roots(), diagnostics)
}

pub(crate) fn client_tls_from_roots(
    roots: Result<TrustRoots, TrustRootsError>,
    diagnostics: TrustDiagnostics,
) -> Result<rustls::ClientConfig, NativeTlsError> {
    const ACCOUNT_LOG: &str = concat!(env!("CARGO_CRATE_NAME"), "::connections::http_client");
    let roots = roots.map_err(|error| {
        if diagnostics == TrustDiagnostics::Account {
            tracing::warn!(target: ACCOUNT_LOG, loader_error = ?error, "account trust roots unavailable");
        }
        match error {
            TrustRootsError::Unsupported => NativeTlsError::Unsupported,
            _ => NativeTlsError::TrustStoreUnavailable,
        }
    })?;
    let mut store = rustls::RootCertStore::empty();
    let (usable, unparsable) =
        store.add_parsable_certificates(roots.certificates.into_iter().map(CertificateDer::from));
    if diagnostics == TrustDiagnostics::Account {
        tracing::info!(
            target: ACCOUNT_LOG,
            kept = usable,
            skipped = roots.skipped,
            unreadable = roots.unreadable,
            unparsable,
            "account trust roots"
        );
    }
    if usable == 0 {
        return Err(NativeTlsError::TrustStoreUnavailable);
    }
    // shortcut: a root Windows hasn't fetched yet fails as UnknownIssuer; revisit if users hit it
    // shortcut: server-supplied intermediate distrust, usage-specific not-before and revocation aren't checked; revisit before broad release
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| NativeTlsError::Configuration)?
    .with_root_certificates(store)
    .with_no_client_auth())
}

/// Loads the usable public roots from the OS trust store.
///
/// # Errors
/// Returns `Unsupported` because this build has no trust-store adapter.
#[cfg(not(windows))]
pub fn trust_roots() -> Result<TrustRoots, TrustRootsError> {
    Err(TrustRootsError::Unsupported)
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn native_roots_require_an_adapter() {
        assert_eq!(
            super::trust_roots(),
            Err(super::TrustRootsError::Unsupported)
        );
    }
}
