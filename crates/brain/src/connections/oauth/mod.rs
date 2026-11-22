//! One-shot loopback authorization, independent of the saved tokens and account state machine.

mod callback;
pub mod exchange;
pub mod id_token;
mod pkce;
pub mod refresh;
pub mod revoke;
#[cfg(test)]
mod tests;

use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use kumochi_protocol::{Secret, constants};
use tokio::net::TcpListener;
use tokio::time::Instant;
use url::Url;
use zeroize::Zeroizing;

use super::records::Account;

/// Public provider contract: `OpenAI` discovery's `authorization_endpoint`.
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
/// Public provider contract: the sign-in guide's initial registration client.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
/// Public provider contract: the sign-in guide's resource and complete requested scope set.
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Local policy: pending authorization lives no longer than five minutes, including replies.
pub(crate) const ATTEMPT_LIMIT: Duration = Duration::from_secs(300);

/// Whether the user explicitly requested another consent screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Ordinary registration or returning sign-in.
    SignIn,
    /// Explicitly enable `ChatGPT` plan use with `prompt=consent`.
    EnablePlan,
}

/// Closed failures; no URL, request, provider text or OS launch diagnostics escape here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignInError {
    /// The local listener failed.
    #[error("sign-in loopback failed")]
    Io,
    /// The attempt's deadline elapsed.
    #[error("sign-in expired")]
    Expired,
    /// A state-matching callback couldn't complete this registration.
    #[error("sign-in failed")]
    SignInFailed,
    /// The user declined consent in a state-matching callback.
    #[error("sign-in declined")]
    Declined,
    /// An unrecognized authorization error was returned; its text is discarded.
    #[error("sign-in authorization failed")]
    Authorization,
}

/// Failures while preparing an attempt, before a callback can be received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    /// The local listener failed.
    #[error("sign-in loopback failed")]
    Io,
    /// The OS random source failed.
    #[error("sign-in randomness unavailable")]
    Random,
}

/// Failures while opening a prepared attempt in the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The attempt has already expired.
    #[error("sign-in expired")]
    Expired,
    /// The OS couldn't open the system browser.
    #[error("sign-in browser launch failed")]
    Launch,
}

/// A token-free URL passed as the sole application-supplied browser argument.
#[derive(Debug)]
pub struct LaunchTarget(Secret<Zeroizing<String>>);

impl LaunchTarget {
    /// Inspect only at the launcher boundary; never log or send this over the pipe.
    pub fn url(&self) -> &str {
        self.0.expose().as_str()
    }
}

/// Browser opening seam: tests record the target as data without opening a browser.
pub trait Launcher {
    /// Open the target without logging it.
    ///
    /// # Errors
    /// Return `LaunchError::Launch` if the operating system refuses to open the browser.
    fn launch(&self, target: &LaunchTarget) -> Result<(), LaunchError>;
}

/// The system browser, opened through webbrowser with its default options.
#[derive(Debug)]
pub struct SystemBrowser;

impl Launcher for SystemBrowser {
    #[expect(
        clippy::disallowed_methods,
        reason = "the attempt's launch admission holds spawn_lock"
    )]
    fn launch(&self, target: &LaunchTarget) -> Result<(), LaunchError> {
        webbrowser::open(target.url()).map_err(|_| LaunchError::Launch)
    }
}

/// A process-local attempt. Dropping it closes the listener; receiving consumes it once.
pub struct SignIn {
    listener: TcpListener,
    redirect: String,
    state: Secret<Zeroizing<String>>,
    nonce: Secret<Zeroizing<String>>,
    verifier: Secret<Zeroizing<String>>,
    target: LaunchTarget,
    saved_client: Option<Secret<Zeroizing<String>>>,
    deadline: Instant,
    #[cfg(test)]
    events: Option<tokio::sync::mpsc::UnboundedSender<callback::Event>>,
}

impl fmt::Debug for SignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignIn(***)")
    }
}

/// Validated callback data for the later token exchange, not a signed-in account.
#[derive(Debug)]
pub struct Authorization {
    /// One-use code. Never log it.
    pub code: Secret<Zeroizing<String>>,
    /// Issued client for this exchange, checked against the pending saved client when present.
    pub client_id: Secret<Zeroizing<String>>,
    /// Nonce to check in the signed ID token, never against callback query parameters.
    pub nonce: Secret<Zeroizing<String>>,
    /// PKCE verifier for the exchange.
    pub verifier: Secret<Zeroizing<String>>,
    /// Exact URI used in the authorization request, including the chosen port.
    pub redirect: String,
}

impl SignIn {
    /// Bind loopback on port 0 and prepare fresh state, nonce and PKCE before any browser opens.
    /// Only the saved client is copied; saved tokens and email never enter the launch target.
    ///
    /// # Errors
    /// Fails if loopback or OS randomness is unavailable.
    ///
    /// # Panics
    /// Panics if the compiled-in authorization endpoint isn't a valid URL.
    pub async fn bind(
        host: &str,
        saved: Option<&Account>,
        intent: Intent,
        limit: Duration,
    ) -> Result<Self, BindError> {
        let deadline = Instant::now() + limit;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| BindError::Io)?;
        let address = listener.local_addr().map_err(|_| BindError::Io)?;
        let redirect = format!("http://{address}/auth/callback");
        let state = Secret::new(pkce::random().map_err(|_| BindError::Random)?);
        let nonce = Secret::new(pkce::random().map_err(|_| BindError::Random)?);
        let verifier = Secret::new(pkce::random().map_err(|_| BindError::Random)?);
        let mut authorization =
            Url::parse(AUTHORIZE).expect("the provider endpoint is a valid URL");
        {
            let mut query = authorization.query_pairs_mut();
            query.extend_pairs([
                (
                    "client_id",
                    saved.map_or(DYNAMIC_CLIENT, |a| a.client_id.as_str()),
                ),
                ("ext_agent_host_id", host),
                ("response_type", "code"),
                ("redirect_uri", &redirect),
                ("scope", SCOPES),
                ("resource", RESOURCE),
                ("state", state.expose()),
                ("nonce", nonce.expose()),
                ("code_challenge_method", "S256"),
                ("code_challenge", &pkce::challenge(verifier.expose())),
            ]);
            if saved.is_none() {
                query.append_pair("agent_name_hint", constants::DISPLAY_NAME);
            }
            if intent == Intent::EnablePlan {
                query.append_pair("prompt", "consent");
            }
        }
        Ok(Self {
            listener,
            redirect,
            state,
            nonce,
            verifier,
            target: LaunchTarget(Secret::new(Zeroizing::new(String::from(authorization)))),
            saved_client: saved.map(|a| Secret::new(a.client_id.clone())),
            deadline,
            #[cfg(test)]
            events: None,
        })
    }

    /// Open the already-bound attempt with the supplied launcher, never after expiry.
    ///
    /// # Errors
    /// Fails if the attempt expired or the launcher refuses the target.
    pub async fn open_browser(
        &self,
        launcher: Arc<dyn Launcher + Send + Sync>,
    ) -> Result<(), LaunchError> {
        let deadline = self.deadline;
        if Instant::now() >= deadline {
            return Err(LaunchError::Expired);
        }
        let alive = Arc::new(());
        let pending = Arc::downgrade(&alive);
        let target = LaunchTarget(self.target.0.clone());
        let launch = tokio::task::spawn_blocking(move || {
            let remaining = || {
                pending.upgrade()?;
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
            };
            let _spawning =
                crate::platform::spawn_lock_until(&remaining).map_err(|_| LaunchError::Expired)?;
            remaining().ok_or(LaunchError::Expired)?;
            launcher.launch(&target)
        });
        let opened = tokio::time::timeout_at(deadline, launch).await;
        drop(alive);
        if Instant::now() >= deadline {
            return Err(LaunchError::Expired);
        }
        opened
            .map_err(|_| LaunchError::Expired)?
            .map_err(|_| LaunchError::Launch)?
    }

    /// Wait for one matching callback. Invalid or stale requests don't consume the attempt.
    /// Drop this future to cancel; the listener and attempt data are then dropped together.
    ///
    /// # Errors
    /// Fails on expiry, listener failure or a state-matching authorization error.
    pub async fn receive(self) -> Result<Authorization, SignInError> {
        if Instant::now() >= self.deadline {
            return Err(SignInError::Expired);
        }
        let (code, client_id) = tokio::time::timeout_at(self.deadline, self.callback())
            .await
            .map_err(|_| SignInError::Expired)??;
        Ok(Authorization {
            code: Secret::new(code),
            client_id: Secret::new(client_id),
            nonce: self.nonce,
            verifier: self.verifier,
            redirect: self.redirect,
        })
    }
}
