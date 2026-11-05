//! The user's own Anthropic key, staged and kept only in Credential Manager.

pub mod key;
pub mod models;

use kumochi_protocol::message::KeyRejection;

use crate::connections::error::VaultError;

/// One serialized operation on the key's exact credential names.
#[derive(Debug, Clone, Copy)]
pub(crate) enum KeyOp {
    Load,
    Adopt,
    Remove,
}

/// A content-free credential or local-format failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum KeyError {
    #[error("the key was refused")]
    Rejected(KeyRejection),
    #[error("the key store is unavailable")]
    Vault(#[from] VaultError),
}
