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
    let store = if cfg!(target_os = "macos") {
        Store::Keychain
    } else {
        Store::Files
    };
    roots_in(
        store,
        |name| std::env::var_os(name).is_some(),
        || {
            let loaded = rustls_native_certs::load_native_certs();
            (loaded.certs, loaded.errors.len())
        },
    )
}

/// Where rustls-native-certs reads roots on this OS.
#[cfg(any(not(windows), test))]
#[derive(Clone, Copy)]
enum Store {
    /// macOS's Keychain trust settings: a variable that would replace them, or any entry that
    /// couldn't be read and might have been a denial, refuses the whole store.
    Keychain,
    /// Linux's bundle files: an entry that couldn't be read is left out and counted.
    Files,
}

/// Applies `store`'s policy to what `load` returns, the roots and how many entries failed;
/// `set` says whether an environment variable is set.
#[cfg(any(not(windows), test))]
fn roots_in(
    store: Store,
    set: impl Fn(&str) -> bool,
    load: impl FnOnce() -> (Vec<CertificateDer<'static>>, usize),
) -> Result<TrustRoots, TrustRootsError> {
    // shortcut: Keychain anchors are read less strictly than SecTrust checks them (Unspecified and non-SSL entries count as roots; hostname and app limits are ignored), until OSC-1136 verifies through SecTrust
    let keychain = matches!(store, Store::Keychain);
    if keychain && STORE_OVERRIDES.into_iter().any(set) {
        return Err(TrustRootsError::Overridden);
    }
    let (certs, failed) = load();
    if keychain && failed > 0 {
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

    use super::{Store, TrustRoots, TrustRootsError, roots_in};

    const DER: [u8; 5] = [0x30, 0x03, 0x02, 0x01, 0x07];

    fn one_root(failed: usize) -> impl FnOnce() -> (Vec<CertificateDer<'static>>, usize) {
        move || (vec![CertificateDer::from(DER.to_vec())], failed)
    }

    fn kept(failed: usize) -> TrustRoots {
        TrustRoots {
            certificates: vec![DER.to_vec()],
            skipped: failed,
            unreadable: failed,
        }
    }

    #[test]
    fn a_store_that_loads_no_root_is_empty() {
        for store in [Store::Keychain, Store::Files] {
            assert_eq!(
                roots_in(store, |_| false, || (Vec::new(), 0)),
                Err(TrustRootsError::Empty)
            );
        }
        assert_eq!(
            roots_in(Store::Files, |_| false, || (Vec::new(), 3)),
            Err(TrustRootsError::Empty)
        );
    }

    #[test]
    fn bundle_files_keep_roots_whole_and_count_failed_entries() {
        assert_eq!(roots_in(Store::Files, |_| false, one_root(2)), Ok(kept(2)));
        assert_eq!(roots_in(Store::Files, |_| true, one_root(0)), Ok(kept(0)));
    }

    #[test]
    fn the_keychain_refuses_any_unreadable_entry() {
        assert_eq!(
            roots_in(Store::Keychain, |_| false, one_root(1)),
            Err(TrustRootsError::Unreadable)
        );
        assert_eq!(
            roots_in(Store::Keychain, |_| false, one_root(0)),
            Ok(kept(0))
        );
    }

    #[test]
    fn either_variable_refuses_the_keychain_before_it_is_read() {
        for variable in ["SSL_CERT_FILE", "SSL_CERT_DIR"] {
            assert_eq!(
                roots_in(
                    Store::Keychain,
                    |name| name == variable,
                    || panic!("the Keychain was read")
                ),
                Err(TrustRootsError::Overridden)
            );
        }
        assert_eq!(
            roots_in(Store::Keychain, |name| name == "SSL_CERT", one_root(0)),
            Ok(kept(0))
        );
    }
}
