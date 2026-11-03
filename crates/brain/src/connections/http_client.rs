//! Account HTTP: manual OS roots, HTTPS only, no redirects or proxies, and bounded bodies.

#[cfg(test)]
mod tests;

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::Secret;
use reqwest::redirect::Policy;
use rustls::pki_types::CertificateDer;
use url::Url;
use zeroize::Zeroizing;

/// The issuer from the public `OpenAI` discovery document.
pub(crate) const ISSUER: &str = "https://auth.openai.com";
/// Local policy: authentication, discovery, JWKS and error bodies share a 1 MiB byte cap.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Closed transport failures; no raw library diagnostics or provider text are retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// No usable OS trust roots could be loaded or parsed.
    #[error("account trust store unavailable")]
    TrustStoreUnavailable,
    /// The TLS or HTTP configuration couldn't be built.
    #[error("account HTTP configuration failed")]
    Configuration,
    /// A URL didn't meet the endpoint's HTTPS/origin rules.
    #[error("account endpoint refused")]
    Endpoint,
    /// A request tried to send credentials to a read-only endpoint.
    #[error("account request refused")]
    Request,
    /// Redirects are never followed.
    #[error("account redirect refused")]
    Redirect,
    /// A bounded request timed out.
    #[error("account request timed out")]
    Timeout,
    /// The connection or response stream failed.
    #[error("account transport failed")]
    Transport,
    /// The body exceeded its local byte bound.
    #[error("account response exceeds its bound")]
    BodyTooLarge,
}

/// An HTTPS JWKS URL validated against the discovery issuer's exact origin.
#[derive(Debug)]
pub struct JwksUrl(Secret<Zeroizing<String>>);

impl JwksUrl {
    /// Validate the public discovery document's URL before any request can use it.
    ///
    /// # Errors
    /// Refuses URLs over 2 KiB, a different origin, userinfo or fragments.
    pub fn parse(value: &str) -> Result<Self, HttpError> {
        if value.len() > 2048 {
            return Err(HttpError::Endpoint);
        }
        let url = Url::parse(value).map_err(|_| HttpError::Endpoint)?;
        if url.scheme() != "https"
            || url.host_str() != Some("auth.openai.com")
            || url.port_or_known_default() != Some(443)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(HttpError::Endpoint);
        }
        Ok(Self(Secret::new(Zeroizing::new(String::from(url)))))
    }
}

/// The only account destinations needed by code exchange and identity verification.
#[derive(Debug)]
pub enum Endpoint {
    /// The documented form-token endpoint; the only endpoint accepting form credentials.
    Token,
    /// The public `OpenID` discovery document.
    Discovery,
    /// A validated same-origin JWKS document, with no credentials.
    Jwks(JwksUrl),
}

impl Endpoint {
    fn url(&self) -> &str {
        match self {
            Self::Token => "https://auth.openai.com/api/accounts/oauth/token",
            Self::Discovery => "https://auth.openai.com/.well-known/openid-configuration",
            Self::Jwks(url) => url.0.expose(),
        }
    }
}

/// Request data for a real transport or an offline scripted fixture.
#[derive(Debug)]
pub struct Request {
    /// A fixed endpoint or a checked discovery result.
    pub endpoint: Endpoint,
    /// Form values are exposed only to the HTTP encoder, never to diagnostics.
    pub fields: Vec<(&'static str, Secret<Zeroizing<String>>)>,
}

/// A bounded response, without provider diagnostics in Debug output.
#[derive(Debug)]
pub struct Response {
    /// The status, retained even when the provider doesn't return OAuth JSON.
    pub status: u16,
    /// A bounded request-id header, when present.
    pub request_id: Option<String>,
    /// Raw response bytes, wiped when their terminal owner drops them.
    pub body: Secret<Zeroizing<Vec<u8>>>,
}

/// Transport seam: fixtures inspect request data and return bounded synthetic responses.
pub trait Http: Send + Sync {
    /// Execute one request without logging request or response contents.
    ///
    /// # Errors
    /// Fails closed on transport, deadline, endpoint, redirect or body-bound violations.
    fn execute(&self, request: Request)
    -> impl Future<Output = Result<Response, HttpError>> + Send;
}

/// The checked account client. Its raw reqwest client never leaves this module.
#[derive(Clone)]
pub struct Client(reqwest::Client);

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountHttpClient")
    }
}

/// Construct the account client using only usable public roots from the OS store.
///
/// # Errors
/// Refuses an unreadable/empty trust store or a failed TLS/HTTP configuration.
pub fn build() -> Result<Client, HttpError> {
    let roots = crate::platform::trust_roots().map_err(|_| HttpError::TrustStoreUnavailable)?;
    from_roots(roots.certificates).map(Client)
}

fn from_roots(roots: Vec<Vec<u8>>) -> Result<reqwest::Client, HttpError> {
    let mut store = rustls::RootCertStore::empty();
    let (usable, _) = store.add_parsable_certificates(roots.into_iter().map(CertificateDer::from));
    if usable == 0 {
        return Err(HttpError::TrustStoreUnavailable);
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| HttpError::Configuration)?
    .with_root_certificates(store)
    .with_no_client_auth();
    // shortcut: proxied networks can't sign in, until there's a user-set proxy setting
    // shortcut: a root Windows hasn't fetched yet fails as UnknownIssuer; revisit if users hit it
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|_| HttpError::Configuration)
}

impl Http for Client {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        let url = Zeroizing::new(request.endpoint.url().to_owned());
        self.send_to(&url, request).await
    }
}

impl Client {
    async fn send_to(&self, url: &str, request: Request) -> Result<Response, HttpError> {
        if !url.starts_with("https://") {
            return Err(HttpError::Endpoint);
        }
        let builder = if matches!(request.endpoint, Endpoint::Token) {
            let fields: Vec<_> = request
                .fields
                .iter()
                .map(|(key, value)| (*key, value.expose().as_str()))
                .collect();
            self.0.post(url).form(&fields)
        } else {
            if !request.fields.is_empty() {
                return Err(HttpError::Request);
            }
            self.0.get(url).timeout(Duration::from_secs(10))
        };
        let mut response = builder
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        if response.status().is_redirection() {
            return Err(HttpError::Redirect);
        }
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 256 && v.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned);
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| transport_error(&error))?
        {
            if chunk.len() > MAX_BODY_BYTES - bytes.len() {
                return Err(HttpError::BodyTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Response {
            status,
            request_id,
            body: Secret::new(bytes),
        })
    }
}

fn transport_error(error: &reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Transport
    }
}
