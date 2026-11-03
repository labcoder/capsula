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
}

impl ErrorKey {
    /// Every key, for the shell's catalog test.
    pub const ALL: [Self; 2] = [Self::SettingsRefused, Self::ConfigUnreadable];
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
        assert_eq!(bytes[1], 4);
        bytes[1] = 5;
        bytes.splice(2..2, [1, 0]);
        assert!(postcard::from_bytes::<UserError>(&bytes).is_err());
    }

    #[test]
    fn the_worst_case_error_fits_the_control_pipe() {
        let frame = frame::encode(&ControlToShell::Error(worst()), Pipe::Control).expect("fits");
        // Tags, then 4 args of a tag and a 5-byte varint, then 3 action tags.
        assert_eq!(frame.len() - HEADER_LEN, 1 + 1 + 1 + 4 * 6 + 1 + 3);
    }
}
