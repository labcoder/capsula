//! Bounded account and revocation records, kept exclusively in the brain's vault.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::error::VaultError;

/// Maximum serialized bytes in either chunked record, including JSON escaping and metadata.
pub const MAX_RECORD_BYTES: usize = 48 * 1024;
/// Maximum bytes in an access or identity token.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// Maximum bytes in a refresh token.
pub const MAX_REFRESH_BYTES: usize = 8 * 1024;
/// Maximum bytes in a client identifier or subject.
pub const MAX_ID_BYTES: usize = 256;
/// Maximum bytes in the granted scope string.
pub const MAX_SCOPE_BYTES: usize = 1024;
/// Maximum bytes in the display email.
pub const MAX_EMAIL_BYTES: usize = 254;
/// Maximum independently owned exchange entries in the revocation list.
pub const MAX_EXCHANGES: usize = 4;

/// A registration and its optional token set; registration-only records survive ordinary sign-out.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// The client issued to this host.
    pub client_id: Zeroizing<String>,
    /// Absent until an identity has been verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Zeroizing<String>>,
    /// Optional display identity, never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<Zeroizing<String>>,
    /// Whether the user accepted plan use.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_enabled: bool,
    /// Absent for a registration-only record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// A token set persisted before it is used or a rotation is acknowledged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokens {
    /// The bearer token.
    pub access_token: Zeroizing<String>,
    /// The rotating refresh token.
    pub refresh_token: Zeroizing<String>,
    /// The identity token, if supplied.
    pub id_token: Option<Zeroizing<String>>,
    /// The granted scopes.
    pub scope: Zeroizing<String>,
    /// Access-token expiry as Unix seconds.
    pub expires_at: u64,
    /// A rotated set whose identity verification still needs to finish.
    pub pending_identity_validation: bool,
}

/// Whether an entry belongs to the account's refresh lineage or to an independent exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// The account token, replaceable only by its own refresh successor.
    Account,
    /// An independent exchange, never replaceable by another exchange.
    Exchange,
}

/// A token retained only for revocation, with its own captured client identifier.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revocable {
    /// The client that owns this token.
    pub client_id: Zeroizing<String>,
    /// The refresh token to revoke.
    pub refresh_token: Zeroizing<String>,
    /// When it entered this list, as Unix seconds.
    pub added_at: u64,
    /// Whether it belongs to the account lineage.
    pub origin: Origin,
}

/// At most four independent exchanges and one account-lineage entry.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevocationList {
    /// Entries remain separate even when their client identifiers match.
    pub entries: Vec<Revocable>,
}

pub(super) trait Record {
    fn validate(&self) -> Result<(), VaultError>;
}

impl Record for Account {
    fn validate(&self) -> Result<(), VaultError> {
        bounded(&self.client_id, MAX_ID_BYTES)?;
        if let Some(subject) = &self.subject {
            bounded(subject, MAX_ID_BYTES)?;
        }
        if let Some(email) = &self.email {
            bounded(email, MAX_EMAIL_BYTES)?;
        }
        if let Some(tokens) = &self.tokens {
            bounded(&tokens.access_token, MAX_TOKEN_BYTES)?;
            bounded(&tokens.refresh_token, MAX_REFRESH_BYTES)?;
            if let Some(id_token) = &tokens.id_token {
                bounded(id_token, MAX_TOKEN_BYTES)?;
            }
            bounded(&tokens.scope, MAX_SCOPE_BYTES)?;
        }
        Ok(())
    }
}

impl Record for RevocationList {
    fn validate(&self) -> Result<(), VaultError> {
        let accounts = self
            .entries
            .iter()
            .filter(|entry| entry.origin == Origin::Account)
            .count();
        if accounts > 1 || self.entries.len().saturating_sub(accounts) > MAX_EXCHANGES {
            return Err(VaultError::Bounds);
        }
        for entry in &self.entries {
            bounded(&entry.client_id, MAX_ID_BYTES)?;
            bounded(&entry.refresh_token, MAX_REFRESH_BYTES)?;
        }
        Ok(())
    }
}

fn bounded(value: &str, max: usize) -> Result<(), VaultError> {
    if value.len() > max {
        return Err(VaultError::Bounds);
    }
    Ok(())
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Account(***)")
    }
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tokens(***)")
    }
}

impl fmt::Debug for Revocable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Revocable(***)")
    }
}

impl fmt::Debug for RevocationList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RevocationList(***)")
    }
}
