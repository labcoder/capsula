//! Inert paste staging primitives; layer 5b supplies authenticated runtime admission.

use kumochi_protocol::Secret;
use std::fmt;
use zeroize::Zeroizing;

use super::{Vault, generation};
use crate::connections::anthropic::{KeyError, key::AnthropicKey};
use crate::connections::authority::{Generation, Kind};
use crate::connections::error::VaultError;
use crate::platform::credential::CredentialStore;

pub(crate) struct PastePlan {
    pub(crate) generation: Generation,
    pub(crate) bytes: usize,
}

impl fmt::Debug for PastePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PastePlan(***)")
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PasteError {
    #[error("the paste is no longer current")]
    Changed,
    #[error("the paste was not saved")]
    Key(#[from] KeyError),
}

impl<S: CredentialStore> Vault<S> {
    /// Mints one fresh paste candidate without changing the committed key.
    ///
    /// # Errors
    /// Bounds for an invalid length, or a content-free authority failure.
    pub(crate) fn begin_anthropic_paste(&mut self, len: usize) -> Result<PastePlan, KeyError> {
        if !(16..=2048).contains(&len) {
            return Err(VaultError::Bounds.into());
        }
        let current = self
            .authority
            .current(Kind::AnthropicKey)
            .map_err(VaultError::from)?;
        let plan = self
            .authority
            .begin(Kind::AnthropicKey, current.revision, len)
            .map_err(VaultError::from)?;
        let reply = PastePlan {
            generation: plan.generation(),
            bytes: plan.layout().bytes(),
        };
        self.paste = Some(plan);
        self.sweep();
        Ok(reply)
    }

    /// Consumes a matching candidate and returns a key only after its activation commits.
    ///
    /// # Errors
    /// Changed for an unknown or consumed id; Key for a failed read, validation or save.
    pub(crate) fn adopt_anthropic_paste(
        &mut self,
        id: Generation,
    ) -> Result<Secret<AnthropicKey>, PasteError> {
        if self
            .paste
            .as_ref()
            .is_none_or(|plan| plan.generation() != id)
        {
            return Err(PasteError::Changed);
        }
        let plan = self.paste.take().expect("the matching paste was checked");
        let bytes = generation::read_pending(&self.store, &self.service, &plan)
            .map_err(VaultError::from)
            .map_err(KeyError::from)?;
        let key = AnthropicKey::parse(bytes).map_err(KeyError::Rejected)?;
        let bytes = Secret::new(Zeroizing::new(key.expose().as_str().as_bytes().to_vec()));
        self.write_expected(Kind::AnthropicKey, plan.base(), &bytes, None)
            .map_err(KeyError::from)?;
        Ok(key)
    }
}
