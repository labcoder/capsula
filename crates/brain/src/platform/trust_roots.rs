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
    /// unreadable, oversized or past the cap. Off Windows, the entries that failed to load.
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
    /// Some of the Keychain's trust settings couldn't be read, so a denial in them could be missed.
    #[error("trust settings unreadable")]
    Unreadable,
    /// `SSL_CERT_FILE` or `SSL_CERT_DIR` is set, which would replace the Keychain's roots.
    #[error("trust store overridden by the environment")]
    Overridden,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeTlsError {
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
        NativeTlsError::TrustStoreUnavailable
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

/// The variables that make rustls-native-certs read their files instead of the OS store.
#[cfg(any(not(windows), test))]
const STORE_OVERRIDES: [&str; 2] = ["SSL_CERT_FILE", "SSL_CERT_DIR"];

/// Loads the roots the OS trusts for TLS: on macOS those the user, admin and system Keychain
/// trust settings mark as roots, on Linux the system bundle, or only the files `SSL_CERT_FILE`
/// and `SSL_CERT_DIR` name when either is set. Nothing is fetched.
///
/// # Errors
/// Returns `Empty` when no root loads. On macOS, also `Overridden` when either variable is set
/// and `Unreadable` when any trust setting couldn't be read.
#[cfg(not(windows))]
pub fn trust_roots() -> Result<TrustRoots, TrustRootsError> {
    // shortcut: Keychain anchors are read less strictly than SecTrust checks them (Unspecified and non-SSL entries count as roots; hostname and app limits are ignored), until OSC-1136 verifies through SecTrust
    let keychain = cfg!(target_os = "macos");
    if keychain && overridden(|name| std::env::var_os(name).is_some()) {
        return Err(TrustRootsError::Overridden);
    }
    let loaded = rustls_native_certs::load_native_certs();
    let failures = if keychain {
        Failures::Refuse
    } else {
        Failures::Skip
    };
    roots_from(&loaded.certs, loaded.errors.len(), failures)
}

/// What a store entry that couldn't be read does to the store.
#[cfg(any(not(windows), test))]
#[derive(Clone, Copy)]
enum Failures {
    /// It's left out, like any root that isn't kept.
    Skip,
    /// It refuses the whole store: the Keychain entry might have been a denial.
    Refuse,
}

#[cfg(any(not(windows), test))]
fn overridden(set: impl Fn(&str) -> bool) -> bool {
    STORE_OVERRIDES.into_iter().any(set)
}

/// Keeps every loaded root whole; `failed` counts the store entries that couldn't be read.
#[cfg(any(not(windows), test))]
fn roots_from(
    certs: &[CertificateDer<'static>],
    failed: usize,
    failures: Failures,
) -> Result<TrustRoots, TrustRootsError> {
    if matches!(failures, Failures::Refuse) && failed > 0 {
        return Err(TrustRootsError::Unreadable);
    }
    if certs.is_empty() {
        return Err(TrustRootsError::Empty);
    }
    Ok(TrustRoots {
        certificates: certs.iter().map(|root| root.to_vec()).collect(),
        skipped: failed,
        unreadable: failed,
    })
}

#[cfg(test)]
mod tests {
    use rustls::pki_types::CertificateDer;

    use super::{Failures, TrustRoots, TrustRootsError, overridden, roots_from};

    const DER: [u8; 5] = [0x30, 0x03, 0x02, 0x01, 0x07];

    #[test]
    fn a_store_that_loads_no_root_is_empty() {
        for failures in [Failures::Skip, Failures::Refuse] {
            assert_eq!(roots_from(&[], 0, failures), Err(TrustRootsError::Empty));
        }
        assert_eq!(
            roots_from(&[], 3, Failures::Skip),
            Err(TrustRootsError::Empty)
        );
    }

    #[test]
    fn loaded_roots_are_kept_whole_and_failed_entries_counted() {
        assert_eq!(
            roots_from(&[CertificateDer::from(DER.to_vec())], 2, Failures::Skip),
            Ok(TrustRoots {
                certificates: vec![DER.to_vec()],
                skipped: 2,
                unreadable: 2,
            })
        );
    }

    #[test]
    fn a_strict_store_with_any_unreadable_entry_is_refused() {
        assert_eq!(
            roots_from(&[CertificateDer::from(DER.to_vec())], 1, Failures::Refuse),
            Err(TrustRootsError::Unreadable)
        );
        assert_eq!(
            roots_from(&[CertificateDer::from(DER.to_vec())], 0, Failures::Refuse),
            Ok(TrustRoots {
                certificates: vec![DER.to_vec()],
                skipped: 0,
                unreadable: 0,
            })
        );
    }

    #[test]
    fn either_variable_overrides_the_store() {
        assert!(!overridden(|_| false));
        assert!(overridden(|name| name == "SSL_CERT_FILE"));
        assert!(overridden(|name| name == "SSL_CERT_DIR"));
        assert!(!overridden(|name| name == "SSL_CERT"));
    }
}
