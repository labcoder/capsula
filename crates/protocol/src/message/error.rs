//! Errors the user sees, sent as a key into the shell's catalog with typed arguments, so no
//! prompt, title or token text can ride along and the tap may record them.

use serde::{Deserialize, Serialize};

/// The most arguments a [`UserError`] carries.
pub const MAX_ERROR_ARGS: usize = 4;
/// The most actions a [`UserError`] offers.
pub const MAX_ERROR_ACTIONS: usize = 3;

/// What went wrong, as the shell's catalog names it. Append only: the tags travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKey {
    /// A settings change couldn't be saved.
    SettingsRefused,
    /// The settings file exists but can't be read, so changes are refused until it's fixed.
    ConfigUnreadable,
    /// The sign-in attempt passed its five minutes.
    SignInTimedOut,
    /// The sign-in couldn't be completed or checked.
    SignInFailed,
    /// The browser signed in to a different account than the saved one.
    SignInOtherAccount,
    /// The browser couldn't be opened.
    BrowserUnavailable,
    /// The account's provider couldn't be reached, or answered that it's temporarily unavailable.
    AccountOffline,
    /// Credential Manager refused to read or save the account.
    CredentialsUnavailable,
    /// Earlier sessions still wait to be ended, so a new sign-in can't start.
    RevocationBacklog,
    /// No usable root certificate loaded from Windows' trusted root store.
    TrustStoreUnavailable,
    /// Plan use wasn't granted.
    PlanNotEnabled,
    /// The app's registration or grant was refused; retrying won't help.
    AccountMisconfigured,
    /// The sign-out couldn't be recorded, so a restart before Retry would sign in again.
    SignOutNotDurable,
    /// The provider ended the session; the saved registration signs in again.
    SessionEnded,
    /// This account can't use its plan here.
    PlanUnavailable,
    /// The plan's usage limit for this app was reached; the usage page says more.
    UsageLimit,
    /// The provider didn't accept the account for this request; signing in again may help.
    AccountRefused,
    /// The provider refused this request as made; retrying it won't help.
    RequestRefused,
    /// The provider's answer couldn't be read.
    ResponseUnreadable,
}

impl ErrorKey {
    /// Every key, for the shell's catalog test.
    pub const ALL: [Self; 19] = [
        Self::SettingsRefused,
        Self::ConfigUnreadable,
        Self::SignInTimedOut,
        Self::SignInFailed,
        Self::SignInOtherAccount,
        Self::BrowserUnavailable,
        Self::AccountOffline,
        Self::CredentialsUnavailable,
        Self::RevocationBacklog,
        Self::TrustStoreUnavailable,
        Self::PlanNotEnabled,
        Self::AccountMisconfigured,
        Self::SignOutNotDurable,
        Self::SessionEnded,
        Self::PlanUnavailable,
        Self::UsageLimit,
        Self::AccountRefused,
        Self::RequestRefused,
        Self::ResponseUnreadable,
    ];
}

/// A typed, content-free argument; there's no text variant on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorArg {
    /// A wait, as `$seconds` in the catalog.
    Seconds(u32),
    /// A number of things, as `$count`.
    Count(u32),
}

/// What the user can do about an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Try the same thing again.
    Retry,
    /// Sign in to the account.
    SignIn,
    /// Turn the feature on.
    Enable,
    /// Open the account's usage page.
    ManageUsage,
    /// Open the settings.
    OpenSettings,
}

/// More arguments or actions than a [`UserError`] allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a user error carries at most {MAX_ERROR_ARGS} arguments and {MAX_ERROR_ACTIONS} actions")]
pub struct TooMany;

/// An error to show the user. Its lengths are checked when it's built and when it's decoded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Unchecked")]
pub struct UserError {
    key: ErrorKey,
    args: Vec<ErrorArg>,
    actions: Vec<Action>,
}

#[derive(Deserialize)]
struct Unchecked {
    key: ErrorKey,
    args: Vec<ErrorArg>,
    actions: Vec<Action>,
}

impl TryFrom<Unchecked> for UserError {
    type Error = TooMany;

    fn try_from(error: Unchecked) -> Result<Self, TooMany> {
        Self::new(error.key, error.args, error.actions)
    }
}

impl From<ErrorKey> for UserError {
    fn from(key: ErrorKey) -> Self {
        Self {
            key,
            args: Vec::new(),
            actions: Vec::new(),
        }
    }
}

impl UserError {
    /// # Errors
    /// [`TooMany`] past [`MAX_ERROR_ARGS`] arguments or [`MAX_ERROR_ACTIONS`] actions.
    pub fn new(key: ErrorKey, args: Vec<ErrorArg>, actions: Vec<Action>) -> Result<Self, TooMany> {
        if args.len() > MAX_ERROR_ARGS || actions.len() > MAX_ERROR_ACTIONS {
            return Err(TooMany);
        }
        Ok(Self { key, args, actions })
    }

    /// What went wrong.
    #[must_use]
    pub fn key(&self) -> ErrorKey {
        self.key
    }

    /// The catalog's arguments.
    #[must_use]
    pub fn args(&self) -> &[ErrorArg] {
        &self.args
    }

    /// What the user can do about it.
    #[must_use]
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Action, ErrorArg, ErrorKey, MAX_ERROR_ACTIONS, MAX_ERROR_ARGS, TooMany, UserError,
    };
    use crate::frame::{self, HEADER_LEN, Pipe};
    use crate::message::ControlToShell;

    fn worst() -> UserError {
        UserError::new(
            ErrorKey::ConfigUnreadable,
            vec![ErrorArg::Count(u32::MAX); MAX_ERROR_ARGS],
            vec![Action::OpenSettings; MAX_ERROR_ACTIONS],
        )
        .expect("at the limits")
    }

    #[test]
    fn new_refuses_more_than_the_limits() {
        let args = vec![ErrorArg::Seconds(1); MAX_ERROR_ARGS + 1];
        let actions = vec![Action::Retry; MAX_ERROR_ACTIONS + 1];
        assert_eq!(
            UserError::new(ErrorKey::SettingsRefused, args, Vec::new()),
            Err(TooMany)
        );
        assert_eq!(
            UserError::new(ErrorKey::SettingsRefused, Vec::new(), actions),
            Err(TooMany)
        );
    }

    #[test]
    fn decoding_refuses_more_than_the_limits() {
        let error = worst();
        let mut bytes = postcard::to_allocvec(&error).expect("encodes");
        assert_eq!(postcard::from_bytes::<UserError>(&bytes), Ok(error));
        // Key, then the args' length: one more arg than allowed.
        let mut args = bytes.clone();
        assert_eq!(args[1], 4);
        args[1] = 5;
        args.splice(2..2, [1, 0]);
        // `TryFrom`'s refusal reaches postcard as a custom error, which keeps no text.
        assert_eq!(
            postcard::from_bytes::<UserError>(&args),
            Err(postcard::Error::SerdeDeCustom)
        );
        // Key, the args' length, four 6-byte args, then the actions' length: one more action.
        let at = 2 + 4 * 6;
        assert_eq!(bytes[at], 3);
        bytes[at] = 4;
        bytes.push(0);
        assert_eq!(
            postcard::from_bytes::<UserError>(&bytes),
            Err(postcard::Error::SerdeDeCustom)
        );
    }

    #[test]
    fn every_key_that_decodes_is_in_all() {
        let decoded: Vec<ErrorKey> = (0..=u8::MAX)
            .filter_map(|tag| postcard::from_bytes(&[tag]).ok())
            .collect();
        assert!(decoded.iter().all(|key| ErrorKey::ALL.contains(key)));
        assert_eq!(decoded.len(), ErrorKey::ALL.len());
    }

    #[test]
    fn the_worst_case_error_fits_the_control_pipe() {
        let frame = frame::encode(&ControlToShell::Error(worst()), Pipe::Control).expect("fits");
        // Tags, then 4 args of a tag and a 5-byte varint, then 3 action tags.
        assert_eq!(frame.len() - HEADER_LEN, 1 + 1 + 1 + 4 * 6 + 1 + 3);
    }
}
