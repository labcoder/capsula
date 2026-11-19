//! Account HTTP: manual OS roots, HTTPS only, no redirects or proxies, and bounded bodies.

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::Secret;
use reqwest::redirect::Policy;
use rustls::pki_types::CertificateDer;
use url::Url;
use zeroize::Zeroizing;

use super::github::GitHubEndpoint;

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
    /// A request was refused before dispatch: fields or credentials its endpoint doesn't take, or a
    /// checked input out of bounds.
    #[error("account request refused")]
    Request,
    /// Redirects are never followed.
    #[error("account redirect refused")]
    Redirect,
    /// A bounded request timed out.
    #[error("account request timed out")]
    Timeout,
    /// The connection couldn't be opened, so nothing was sent.
    #[error("account connection failed")]
    Connect,
    /// The connection or response stream failed.
    #[error("account transport failed")]
    Transport,
    /// The body exceeded its local byte bound.
    #[error("account response exceeds its bound")]
    BodyTooLarge,
}

/// An HTTPS URL from discovery, such as the JWKS or revocation endpoint, validated against the
/// issuer's exact origin.
#[derive(Debug, Clone)]
pub struct IssuerUrl(Secret<Zeroizing<String>>);

impl IssuerUrl {
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

/// The only account destinations: `OpenAI`'s code exchange, identity verification, revocation and
/// model list, and the typed GitHub operations.
pub enum Endpoint {
    /// The documented form-token endpoint, one of the two accepting form credentials.
    Token,
    /// The public `OpenID` discovery document.
    Discovery,
    /// A validated same-origin JWKS document, with no credentials.
    Jwks(IssuerUrl),
    /// The discovered same-origin revocation endpoint, the other one taking form credentials.
    Revocation(IssuerUrl),
    /// The account's model list, the only destination given the bearer.
    Models(Secret<Zeroizing<String>>),
    /// A typed GitHub operation with its own method, credential and payload.
    GitHub(GitHubEndpoint),
}

impl Endpoint {
    pub(crate) fn url(&self) -> Result<Cow<'_, str>, HttpError> {
        Ok(Cow::Borrowed(match self {
            Self::Token => "https://auth.openai.com/api/accounts/oauth/token",
            Self::Discovery => "https://auth.openai.com/.well-known/openid-configuration",
            Self::Jwks(url) | Self::Revocation(url) => url.0.expose(),
            Self::Models(_) => "https://api.openai.com/v1/models",
            Self::GitHub(endpoint) => return endpoint.url().map(|url| Cow::Owned(url.into())),
        }))
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Token => "Endpoint::Token",
            Self::Discovery => "Endpoint::Discovery",
            Self::Jwks(_) => "Endpoint::Jwks",
            Self::Revocation(_) => "Endpoint::Revocation",
            Self::Models(_) => "Endpoint::Models",
            Self::GitHub(endpoint) => return endpoint.fmt(f),
        })
    }
}

/// Request data for a real transport or an offline scripted fixture.
pub struct Request {
    /// A fixed endpoint or a checked discovery result.
    pub endpoint: Endpoint,
    /// Form values are exposed only to the HTTP encoder, never to diagnostics.
    pub fields: Vec<(&'static str, Secret<Zeroizing<String>>)>,
}

/// A bounded response, without provider diagnostics in Debug output.
pub struct Response {
    /// The status, retained even when the provider doesn't return OAuth JSON.
    pub status: u16,
    /// A bounded request-id header, when present.
    pub request_id: Option<String>,
    /// Parsed content-free rate and server-clock metadata; never raw headers.
    pub meta: ResponseMeta,
    /// Raw response bytes, wiped when their terminal owner drops them.
    pub body: Secret<Zeroizing<Vec<u8>>>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("endpoint", &self.endpoint)
            .field("fields", &self.fields.len())
            .finish()
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field(
                "request_id_bytes",
                &self.request_id.as_ref().map(String::len),
            )
            .field("meta", &self.meta)
            .field("body_bytes", &self.body.expose().len())
            .finish()
    }
}

/// Bounded numeric metadata, absent when its header is duplicated or invalid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResponseMeta {
    /// Server Date as nonnegative Unix seconds, never a local-clock substitute.
    pub date_s: Option<u64>,
    /// Retry delay in seconds, using a valid Date for HTTP-date values.
    pub retry_after_s: Option<u64>,
    /// Whether any Retry-After header arrived, even when its advice was unusable.
    pub retry_after_present: bool,
    /// Remaining requests in the current rate window.
    pub rate_remaining: Option<u64>,
    /// Rate-window reset as nonnegative Unix seconds.
    pub rate_reset_s: Option<u64>,
}

impl ResponseMeta {
    fn parse(headers: &reqwest::header::HeaderMap) -> Self {
        let single = |name: &'static str, max: usize| {
            let mut values = headers.get_all(name).iter();
            let value = values.next()?.to_str().ok()?;
            (values.next().is_none() && value.len() <= max && value.is_ascii()).then_some(value)
        };
        let date_s = single("date", 64).and_then(http_seconds);
        let retry_after_s = single("retry-after", 64).and_then(|value| {
            decimal(value).or_else(|| Some(http_seconds(value)?.saturating_sub(date_s?)))
        });
        Self {
            date_s,
            retry_after_s,
            retry_after_present: headers.contains_key("retry-after"),
            rate_remaining: single("x-ratelimit-remaining", 20).and_then(decimal),
            rate_reset_s: single("x-ratelimit-reset", 20).and_then(decimal),
        }
    }
}

fn decimal(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn http_seconds(value: &str) -> Option<u64> {
    if !value.ends_with(" GMT") {
        return None;
    }
    let date =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822).ok()?;
    // time's RFC 2822 parser doesn't check the weekday against the date.
    let weekday = date.weekday().to_string();
    if !value.trim_start().starts_with(weekday.get(..3)?) {
        return None;
    }
    u64::try_from(date.unix_timestamp()).ok()
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

/// A shareable [`Http`], so the runtime can hand one transport to tasks without naming its type.
#[derive(Clone)]
pub struct SharedHttp(Arc<dyn ErasedHttp>);

impl SharedHttp {
    /// Shares `http`.
    pub fn new(http: impl Http + 'static) -> Self {
        Self(Arc::new(http))
    }
}

impl fmt::Debug for SharedHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedHttp")
    }
}

impl Http for SharedHttp {
    fn execute(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, HttpError>> + Send {
        self.0.execute_boxed(request)
    }
}

type Pending<'a> = std::pin::Pin<Box<dyn Future<Output = Result<Response, HttpError>> + Send + 'a>>;

/// [`Http`] as a trait object.
trait ErasedHttp: Send + Sync {
    fn execute_boxed(&self, request: Request) -> Pending<'_>;
}

impl<T: Http> ErasedHttp for T {
    fn execute_boxed(&self, request: Request) -> Pending<'_> {
        Box::pin(self.execute(request))
    }
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
    from_roots(crate::platform::trust_roots()).map(Client)
}

fn from_roots(
    roots: Result<crate::platform::TrustRoots, crate::platform::TrustRootsError>,
) -> Result<reqwest::Client, HttpError> {
    connecting_within(roots, Duration::from_secs(15))
}

/// The client, with `connect` as its deadline for the connection and TLS handshake.
fn connecting_within(
    roots: Result<crate::platform::TrustRoots, crate::platform::TrustRootsError>,
    connect: Duration,
) -> Result<reqwest::Client, HttpError> {
    let tls = client_config(roots)?;
    // shortcut: proxied networks can't sign in, until there's a user-set proxy setting
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        // Inert today (no h2 or h3 in this build); pins no retries so enabling http2 can't resend.
        .retry(reqwest::retry::never())
        .connect_timeout(connect)
        .read_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|_| HttpError::Configuration)
}

/// TLS through ring with the usable public roots from `roots`; the model connection uses it too.
pub(crate) fn client_config(
    roots: Result<crate::platform::TrustRoots, crate::platform::TrustRootsError>,
) -> Result<rustls::ClientConfig, HttpError> {
    let roots = roots.map_err(|error| {
        tracing::warn!(loader_error = ?error, "account trust roots unavailable");
        HttpError::TrustStoreUnavailable
    })?;
    let mut store = rustls::RootCertStore::empty();
    let (usable, unparsable) =
        store.add_parsable_certificates(roots.certificates.into_iter().map(CertificateDer::from));
    tracing::info!(
        kept = usable,
        skipped = roots.skipped,
        unreadable = roots.unreadable,
        unparsable,
        "account trust roots"
    );
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
    // shortcut: a root Windows hasn't fetched yet fails as UnknownIssuer; revisit if users hit it
    // shortcut: server-supplied intermediate distrust, usage-specific not-before and revocation aren't checked; revisit before broad release
    Ok(tls)
}

impl Http for Client {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        let url = Zeroizing::new(request.endpoint.url()?.into_owned());
        self.send_to(&url, request).await
    }
}

impl Client {
    fn github_request(
        &self,
        endpoint: &GitHubEndpoint,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        let request = match endpoint {
            GitHubEndpoint::DeviceCode { client_id } => {
                self.0.post(url).form(&[("client_id", client_id.as_str())])
            }
            GitHubEndpoint::AccessToken { client_id, code } => self.0.post(url).form(&[
                ("client_id", client_id.as_str()),
                ("device_code", code.expose().as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ]),
            GitHubEndpoint::RefreshToken { client_id, refresh } => self.0.post(url).form(&[
                ("client_id", client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.expose().as_str()),
            ]),
            GitHubEndpoint::CreateIssue { token, body, .. } => self
                .0
                .post(url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    bearer(token.expose().as_str())?,
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.expose().as_bytes().to_vec()),
            GitHubEndpoint::User { token }
            | GitHubEndpoint::Installations { token, .. }
            | GitHubEndpoint::Repositories { token, .. }
            | GitHubEndpoint::Issues { token, .. } => self.0.get(url).header(
                reqwest::header::AUTHORIZATION,
                bearer(token.expose().as_str())?,
            ),
        }
        .header(
            reqwest::header::USER_AGENT,
            kumochi_protocol::constants::HTTP_USER_AGENT,
        )
        .timeout(endpoint.deadline());
        if matches!(
            endpoint,
            GitHubEndpoint::DeviceCode { .. }
                | GitHubEndpoint::AccessToken { .. }
                | GitHubEndpoint::RefreshToken { .. }
        ) {
            Ok(request.header(reqwest::header::ACCEPT, "application/json"))
        } else {
            Ok(request
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2026-03-10"))
        }
    }
    async fn send_to(&self, url: &str, request: Request) -> Result<Response, HttpError> {
        if !url.starts_with("https://") {
            return Err(HttpError::Endpoint);
        }
        let builder = if let Endpoint::GitHub(endpoint) = &request.endpoint {
            if !request.fields.is_empty() {
                return Err(HttpError::Request);
            }
            self.github_request(endpoint, url)?
        } else if matches!(request.endpoint, Endpoint::Token | Endpoint::Revocation(_)) {
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
            match &request.endpoint {
                Endpoint::Models(token) => self
                    .0
                    .get(url)
                    .header(reqwest::header::AUTHORIZATION, bearer(token.expose())?),
                _ => self.0.get(url).timeout(Duration::from_secs(10)),
            }
        };
        #[cfg(all(windows, debug_assertions))]
        if let Some(observation) = super::model::text_observation::Observation::active() {
            observation.auxiliary(&request.endpoint);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        let github = match &request.endpoint {
            Endpoint::GitHub(endpoint) => Some(endpoint),
            _ => None,
        };
        read_response(response, github).await
    }
}

fn bearer(token: &str) -> Result<reqwest::header::HeaderValue, HttpError> {
    let value = Zeroizing::new(format!("Bearer {token}"));
    let mut header =
        reqwest::header::HeaderValue::from_str(&value).map_err(|_| HttpError::Request)?;
    header.set_sensitive(true);
    Ok(header)
}

fn body_buffer(length: Option<u64>) -> Zeroizing<Vec<u8>> {
    let capacity = length.map_or(0, |length| {
        usize::try_from(length)
            .unwrap_or(MAX_BODY_BYTES)
            .min(MAX_BODY_BYTES)
    });
    Zeroizing::new(Vec::with_capacity(capacity))
}

async fn read_response(
    mut response: reqwest::Response,
    github: Option<&GitHubEndpoint>,
) -> Result<Response, HttpError> {
    if response.status().is_redirection() {
        return Err(HttpError::Redirect);
    }
    let status = response.status().as_u16();
    let meta = ResponseMeta::parse(response.headers());
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 256 && v.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned);
    if matches!(github, Some(GitHubEndpoint::CreateIssue { .. })) && (400..500).contains(&status) {
        return Ok(Response {
            status,
            request_id,
            meta,
            body: Secret::new(Zeroizing::new(Vec::new())),
        });
    }
    let limit = github.map_or(MAX_BODY_BYTES, GitHubEndpoint::body_limit);
    let length_cap = u64::try_from(limit).unwrap_or(u64::MAX);
    let mut bytes = body_buffer(response.content_length().map(|n| n.min(length_cap)));
    // Never Connect: classify reads Connect as "no tokens issued", and a body comes after sending.
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            HttpError::Timeout
        } else {
            HttpError::Transport
        }
    })? {
        if chunk.len() > limit - bytes.len() {
            return Err(HttpError::BodyTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Response {
        status,
        request_id,
        meta,
        body: Secret::new(bytes),
    })
}

fn transport_error(error: &reqwest::Error) -> HttpError {
    // A connect deadline is both: nothing was sent, so the connection decides.
    if error.is_connect() {
        HttpError::Connect
    } else if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Transport
    }
}
