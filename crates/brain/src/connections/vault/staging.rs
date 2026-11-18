//! Authority-owned paste staging reached through authenticated runtime admission.

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
    pub(crate) base: u64,
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
        if !kumochi_protocol::message::ANTHROPIC_KEY_BYTES.contains(&len) {
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
            base: plan.base(),
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
    ) -> Result<(u64, Secret<AnthropicKey>), PasteError> {
        self.adopt_with_clock(id, &generation::System)
    }

    pub(crate) fn adopt_with_clock(
        &mut self,
        id: Generation,
        clock: &dyn generation::Clock,
    ) -> Result<(u64, Secret<AnthropicKey>), PasteError> {
        let plan = self
            .paste
            .take_if(|plan| plan.generation() == id)
            .ok_or(PasteError::Changed)?;
        let parsed = generation::read_pending(&self.store, &self.service, &plan, clock)
            .map_err(VaultError::from)
            .map_err(KeyError::from)
            .and_then(|bytes| AnthropicKey::parse(bytes).map_err(KeyError::Rejected));
        let key = match parsed {
            Ok(key) => key,
            Err(error) => {
                generation::delete_pending(
                    &self.store,
                    &self.service,
                    Kind::AnthropicKey,
                    plan.generation(),
                    plan.layout(),
                );
                return Err(error.into());
            }
        };
        let bytes = Secret::new(Zeroizing::new(key.expose().as_str().as_bytes().to_vec()));
        if let Err(error) = self.write_expected(Kind::AnthropicKey, plan.base(), &bytes, None) {
            generation::delete_pending(
                &self.store,
                &self.service,
                Kind::AnthropicKey,
                plan.generation(),
                plan.layout(),
            );
            return Err(KeyError::from(error).into());
        }
        Ok((plan.base() + 1, key))
    }
}
