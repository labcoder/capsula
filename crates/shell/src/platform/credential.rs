//! Stages a validated paste for the brain, without putting it on the pipe.

use kumochi_protocol::Secret;
use kumochi_protocol::constants::{ANTHROPIC_STAGING_ACCOUNT, CREDENTIAL_SERVICE};
use kumochi_protocol::message::{KeyRejection, validate_anthropic_key};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub(crate) enum StageError {
    #[error("the key was refused")]
    Rejected(KeyRejection),
    #[error("the credential store is unavailable")]
    Store,
}

/// Runs on a background worker; the caller sends `AnthropicKeyStaged` only after success.
#[expect(dead_code, reason = "slice 8 C7 wires the Accounts paste control")]
pub(crate) fn stage_anthropic_key(key: Secret<Zeroizing<String>>) -> Result<(), StageError> {
    let trimmed = validate_anthropic_key(key.expose()).map_err(StageError::Rejected)?;
    let result = keyring::Entry::new(CREDENTIAL_SERVICE, ANTHROPIC_STAGING_ACCOUNT)
        .and_then(|entry| entry.set_secret(trimmed.as_bytes()))
        .map_err(|_| StageError::Store);
    drop(key);
    result
}
