//! The bounded credential record; serialization never carries it onto a pipe.

use super::{GitHubToken, api::GitHubUser, device::Account};
use crate::connections::{error::VaultError, vault};
use crate::platform::credential::SecretBytes;
use kumochi_protocol::Secret;
use kumochi_protocol::message::GitHubLogin;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use zeroize::Zeroizing;

pub(crate) const MAX_BYTES: usize = 2048;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    token: Zeroizing<String>,
    login: GitHubLogin,
    id: NonZeroU64,
}

pub(crate) fn encode(account: &Account) -> Result<SecretBytes, VaultError> {
    vault::encode(
        &Record {
            token: Zeroizing::new(account.token.expose().as_str().to_owned()),
            login: account.user.login.clone(),
            id: NonZeroU64::new(account.user.id).ok_or(VaultError::Unreadable)?,
        },
        MAX_BYTES,
    )
}

pub(crate) fn decode(bytes: &SecretBytes) -> Result<Account, VaultError> {
    let record: Record = vault::decode(bytes, MAX_BYTES)?;
    Ok(Account {
        token: Secret::new(GitHubToken::new(record.token).map_err(|_| VaultError::Unreadable)?),
        user: GitHubUser {
            login: record.login,
            id: record.id.get(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(value: &[u8]) -> SecretBytes {
        Secret::new(Zeroizing::new(value.to_vec()))
    }

    #[test]
    fn the_record_is_bounded_and_rejects_unchecked_credentials_and_identity() {
        let valid = br#"{"token":"ghu_fake_sentinel","login":"octocat","id":7}"#;
        let mut padded = valid.to_vec();
        padded.resize(2048, b' ');
        let account = decode(&bytes(&padded)).unwrap();
        assert_eq!(account.user.id, 7);
        assert_eq!(account.user.login.as_str(), "octocat");
        assert_eq!(account.token.expose().as_str(), "ghu_fake_sentinel");
        assert_eq!(encode(&account).unwrap().expose().as_slice(), valid);
        padded.push(b' ');
        assert!(matches!(decode(&bytes(&padded)), Err(VaultError::Bounds)));
        for bad in [
            "",
            "{",
            "null",
            "[]",
            r#"{"token":"ghu_x","login":"octocat","id":0}"#,
            r#"{"token":"ghu_x","login":"","id":1}"#,
            r#"{"token":"ghu_x","login":"octocat","id":"1"}"#,
            r#"{"token":"ghu_","login":"octocat","id":1}"#,
            r#"{"token":"ghp_x","login":"octocat","id":1}"#,
            r#"{"token":"ghu_x y","login":"octocat","id":1}"#,
            r#"{"token":"ghu_x","login":"octocat","id":1,"id":2}"#,
            r#"{"token":"ghu_x","login":"octocat","id":1,"extra":true}"#,
            r#"{"token":"ghu_x","login":"octocat"}"#,
        ] {
            assert!(matches!(
                decode(&bytes(bad.as_bytes())),
                Err(VaultError::Unreadable)
            ));
        }
        for length in [255, 256] {
            let token = format!("ghu_{}", "x".repeat(length - 4));
            let wire =
                format!(r#"{{"token":"{token}","login":"octocat","id":18446744073709551615}}"#);
            match decode(&bytes(wire.as_bytes())) {
                Ok(account) => {
                    assert_eq!(length, 255);
                    assert_eq!(account.user.id, u64::MAX);
                }
                Err(VaultError::Unreadable) => assert_eq!(length, 256),
                other => panic!("unexpected record result: {other:?}"),
            }
        }
    }
}
