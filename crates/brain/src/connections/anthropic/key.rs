//! Validated API keys, owned only in zeroizing, redacted storage.

use std::fmt;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{KeyRejection, validate_anthropic_key};
use zeroize::Zeroizing;

use crate::platform::credential::SecretBytes;

/// An API key whose private storage is wiped on drop.
#[derive(Clone)]
pub struct AnthropicKey(Zeroizing<String>);

impl AnthropicKey {
    /// Validate credential-store bytes with the same rule the shell uses before staging.
    ///
    /// # Errors
    /// Refuses invalid UTF-8, malformed keys, subscription tokens and admin keys.
    pub fn parse(bytes: SecretBytes) -> Result<Secret<Self>, KeyRejection> {
        let text = std::str::from_utf8(bytes.expose()).map_err(|_| KeyRejection::Format)?;
        let key = validate_anthropic_key(text)?;
        let key = Zeroizing::new(key.to_owned());
        drop(bytes);
        Ok(Secret::new(Self(key)))
    }

    /// Borrow the key only at the credential-store or HTTP boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AnthropicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AnthropicKey(***)")
    }
}

#[cfg(test)]
#[path = "key_tests.rs"]
mod tests;
