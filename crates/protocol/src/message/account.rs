//! The `ChatGPT` account as the shell sees it: content-free operations, and a view whose only
//! identity is the display email. No token, code or account identifier crosses the pipe.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The provider's usage page, which the shell opens itself; the UI guidelines name it.
pub const MANAGE_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// The longest display email, in bytes.
pub const MAX_EMAIL_BYTES: usize = 254;

/// What the user asked of the account. Every op is answered promptly with an
/// [`super::ControlToShell::Account`], a refused one included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountOp {
    /// Start a sign-in, replacing one still waiting for the browser.
    SignIn,
    /// Stop waiting for the browser; the view returns to where the attempt started.
    CancelSignIn,
    /// Stop using the account and remove its saved sign-in.
    SignOut,
    /// Sign in again, asking for plan consent.
    EnablePlan,
    /// Try again whatever the view shows as stuck.
    Retry,
}

/// Where the account stands. It's authoritative: the shell never moves on its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountView {
    /// No usable account.
    SignedOut {
        /// The provider didn't confirm that the last sign-out ended the session.
        revocation_unconfirmed: bool,
    },
    /// Waiting for the browser, or for the sign-in to be checked and saved.
    SigningIn,
    /// Use has stopped and cleanup is under way; no sign-in is offered.
    SigningOut {
        /// The local cleanup's progress.
        cleanup: Cleanup,
        /// The provider may not have ended the session.
        revocation_unconfirmed: bool,
    },
    /// Another Windows session owns the account.
    Busy,
    /// A usable account.
    SignedIn {
        /// The signed display email, when the identity carries one.
        email: Option<AccountEmail>,
        /// Whether requests may use the `ChatGPT` plan.
        plan: PlanState,
        /// What's stuck, if anything.
        recovery: Option<AccountRecovery>,
    },
}

/// A sign-out's local cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cleanup {
    /// Cleanup is running.
    Running,
    /// Credential Manager refused part of it; Retry or the next start tries again.
    Refused,
    /// The sign-out couldn't be recorded, so a restart before Retry signs in again.
    NotDurable,
}

/// Whether the account's plan may be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanState {
    /// The grant includes plan use.
    Enabled,
    /// The user didn't grant plan use.
    Declined,
    /// Plan use isn't available for this account or workspace.
    Unavailable,
}

/// What a signed-in account needs from the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountRecovery {
    /// Something temporary failed; Retry tries again.
    Retry,
    /// The session ended; signing in again keeps the registration.
    SignInAgain,
}

/// A display email: at most [`MAX_EMAIL_BYTES`] bytes on one line, checked on decode too. Its
/// `Debug` prints only a length.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountEmail(String);

/// The text isn't a display email.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a display email")]
pub struct EmailError;

impl AccountEmail {
    /// # Errors
    /// [`EmailError`] for empty text, more than [`MAX_EMAIL_BYTES`] bytes, or a control
    /// character or line break.
    pub fn new(text: &str) -> Result<Self, EmailError> {
        if text.is_empty()
            || text.len() > MAX_EMAIL_BYTES
            || text
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        {
            return Err(EmailError);
        }
        Ok(Self(text.to_owned()))
    }

    /// The email's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AccountEmail {
    type Error = EmailError;

    fn try_from(text: String) -> Result<Self, EmailError> {
        Self::new(&text)
    }
}

impl From<AccountEmail> for String {
    fn from(email: AccountEmail) -> String {
        email.0
    }
}

impl fmt::Debug for AccountEmail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountEmail(len={})", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountEmail, EmailError};

    #[test]
    fn an_email_is_one_bounded_line_on_decode_too() {
        let longest = format!("{}@b", "é".repeat(126));
        let email = AccountEmail::new(&longest).unwrap();
        assert_eq!(email.as_str(), longest);
        let wire = postcard::to_allocvec(&longest).unwrap();
        assert_eq!(postcard::from_bytes::<AccountEmail>(&wire), Ok(email));
        let over = format!("{longest}c");
        for bad in ["", "a\n@b", "a\r@b", "a\u{2028}@b", over.as_str()] {
            assert_eq!(AccountEmail::new(bad), Err(EmailError), "{bad:?}");
            let wire = postcard::to_allocvec(bad).unwrap();
            assert_eq!(
                postcard::from_bytes::<AccountEmail>(&wire),
                Err(postcard::Error::SerdeDeCustom),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_email_debug_shows_only_its_length() {
        let debug = |address| format!("{:?}", AccountEmail::new(address).unwrap());
        let address = "person@example.invalid";
        let other = "writers@demo.testplace";
        assert_eq!(address.len(), other.len());
        let shown = debug(address);
        assert_eq!(shown, debug(other));
        assert!(!shown.contains(address));
        assert!(!shown.contains(format!("{:?}", address.as_bytes()).trim_matches(['[', ']'])));
        let shorter = debug("é@sample.test");
        assert_ne!(shown, shorter);
        for (shown, expected) in [(shown, "22"), (shorter, "14")] {
            assert!(
                shown
                    .split(|c: char| !c.is_ascii_digit() && c != '-')
                    .any(|n| n == expected)
            );
        }
    }
}
