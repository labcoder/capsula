//! The root certificates the account client trusts, read from the operating system's store.

use std::fmt;
use std::io;
#[cfg(any(not(target_os = "macos"), test))]
use std::sync::Arc;

#[cfg(any(not(target_os = "macos"), test))]
use rustls::pki_types::CertificateDer;
use tokio_rustls::TlsConnector;

#[cfg(target_os = "macos")]
#[path = "macos/tls_boundary/mod.rs"]
mod mac_tls;

#[cfg(target_os = "macos")]
pub use mac_tls::NativeStream;
/// A TLS stream from the platform's normal connector.
#[cfg(not(target_os = "macos"))]
pub type NativeStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

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

#[cfg(not(target_os = "macos"))]
pub(crate) fn native_client_tls(
    diagnostics: TrustDiagnostics,
) -> Result<rustls::ClientConfig, NativeTlsError> {
    client_tls_from_roots(super::trust_roots(), diagnostics)
}

#[cfg(target_os = "macos")]
pub(crate) fn native_client_tls(
    _: TrustDiagnostics,
) -> Result<rustls::ClientConfig, NativeTlsError> {
    reject_mac_overrides(|name| std::env::var_os(name).is_some())
        .map_err(|_| NativeTlsError::TrustStoreUnavailable)?;
    mac_tls::config().map_err(|_| NativeTlsError::Configuration)
}

#[cfg(target_os = "macos")]
pub(crate) fn connect_native_tls<'a>(
    tls: &'a TlsConnector,
    host: &'static str,
) -> impl Future<Output = io::Result<NativeStream>> + Send + 'a {
    Box::pin(mac_tls::direct(tls, host))
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn connect_native_tls(
    tls: &TlsConnector,
    host: &'static str,
) -> io::Result<NativeStream> {
    let tcp = tokio::net::TcpStream::connect((host, 443)).await?;
    tcp.set_nodelay(true)?;
    let name = rustls::pki_types::ServerName::try_from(host).map_err(io::Error::other)?;
    tls.connect(name, tcp).await
}

pub(crate) fn native_http_builder(
    diagnostics: TrustDiagnostics,
) -> Result<reqwest::ClientBuilder, NativeTlsError> {
    native_client_tls(diagnostics).map(http_builder)
}

fn http_builder(tls: rustls::ClientConfig) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().use_preconfigured_tls(tls);
    #[cfg(target_os = "macos")]
    let builder = mac_tls::http(builder);
    builder
}

#[cfg(any(not(target_os = "macos"), test))]
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

#[cfg(any(target_os = "macos", test))]
fn reject_mac_overrides(set: impl Fn(&str) -> bool) -> Result<(), TrustRootsError> {
    if ["SSL_CERT_FILE", "SSL_CERT_DIR"].into_iter().any(set) {
        return Err(TrustRootsError::Overridden);
    }
    Ok(())
}

/// Loads system bundle roots or the files named by `SSL_CERT_FILE` and `SSL_CERT_DIR`.
/// Nothing is fetched.
///
/// # Errors
/// Returns `Empty` when no root loads.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn trust_roots() -> Result<TrustRoots, TrustRootsError> {
    let loaded = rustls_native_certs::load_native_certs();
    roots_in(&loaded.certs, loaded.errors.len())
}

#[cfg(any(not(any(windows, target_os = "macos")), test))]
fn roots_in(certs: &[CertificateDer<'_>], failed: usize) -> Result<TrustRoots, TrustRootsError> {
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

    use super::{TrustRoots, TrustRootsError, reject_mac_overrides, roots_in};

    const DER: [u8; 5] = [0x30, 0x03, 0x02, 0x01, 0x07];

    fn kept(failed: usize) -> TrustRoots {
        TrustRoots {
            certificates: vec![DER.to_vec()],
            skipped: failed,
            unreadable: failed,
        }
    }

    #[test]
    fn a_store_that_loads_no_root_is_empty() {
        assert_eq!(roots_in(&[], 0), Err(TrustRootsError::Empty));
        assert_eq!(roots_in(&[], 3), Err(TrustRootsError::Empty));
    }

    #[test]
    fn bundle_files_keep_roots_whole_and_count_failed_entries() {
        let roots = [CertificateDer::from(DER.to_vec())];
        assert_eq!(roots_in(&roots, 2), Ok(kept(2)));
        assert_eq!(roots_in(&roots, 0), Ok(kept(0)));
    }

    #[test]
    fn either_variable_refuses_the_keychain_before_it_is_read() {
        for variable in ["SSL_CERT_FILE", "SSL_CERT_DIR"] {
            assert_eq!(
                reject_mac_overrides(|name| name == variable),
                Err(TrustRootsError::Overridden)
            );
        }
        assert_eq!(reject_mac_overrides(|name| name == "SSL_CERT"), Ok(()));
    }
}
