//! The bounded credential record; serialization never carries it onto a pipe.

use super::{
    GitHubToken, RefreshToken,
    api::{GitHubUser, Tokens},
    device::Account,
};
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
    refresh: Zeroizing<String>,
    expires_at_s: u64,
    refresh_expires_at_s: u64,
    refresh_sent: bool,
    login: GitHubLogin,
    id: NonZeroU64,
}

pub(crate) fn encode(account: &Account) -> Result<SecretBytes, VaultError> {
    if !valid_expiry(
        account.credentials.expires_at_s,
        account.credentials.refresh_expires_at_s,
    ) {
        return Err(VaultError::Unreadable);
    }
    vault::encode(
        &Record {
            token: Zeroizing::new(account.credentials.token.expose().as_str().to_owned()),
            refresh: Zeroizing::new(account.credentials.refresh.expose().as_str().to_owned()),
            expires_at_s: account.credentials.expires_at_s,
            refresh_expires_at_s: account.credentials.refresh_expires_at_s,
            refresh_sent: account.refresh_sent,
            login: account.user.login.clone(),
            id: NonZeroU64::new(account.user.id).ok_or(VaultError::Unreadable)?,
        },
        MAX_BYTES,
    )
}

pub(crate) fn decode(bytes: &SecretBytes) -> Result<Account, VaultError> {
    let record: Record = vault::decode(bytes, MAX_BYTES)?;
    if !valid_expiry(record.expires_at_s, record.refresh_expires_at_s) {
        return Err(VaultError::Unreadable);
    }
    Ok(Account {
        credentials: Tokens {
            token: Secret::new(GitHubToken::new(record.token).map_err(|_| VaultError::Unreadable)?),
            refresh: Secret::new(
                RefreshToken::new(record.refresh).map_err(|_| VaultError::Unreadable)?,
            ),
            expires_at_s: record.expires_at_s,
            refresh_expires_at_s: record.refresh_expires_at_s,
        },
        user: GitHubUser {
            login: record.login,
            id: record.id.get(),
        },
        refresh_sent: record.refresh_sent,
    })
}

fn valid_expiry(access: u64, refresh: u64) -> bool {
    access > 0
        && refresh > 0
        && access.saturating_sub(refresh) <= super::api::MAX_ACCESS_LIFETIME_S
        && refresh.saturating_sub(access) <= super::api::MAX_REFRESH_LIFETIME_S
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fmt::Write;

    const VALID: &[u8] = br#"{"token":"ghu_fake_sentinel","refresh":"ghr_fake_sentinel","expires_at_s":1700028800,"refresh_expires_at_s":1715897600,"refresh_sent":false,"login":"octocat","id":7}"#;

    fn bytes(value: &[u8]) -> SecretBytes {
        Secret::new(Zeroizing::new(value.to_vec()))
    }

    fn valid() -> Value {
        serde_json::from_slice(VALID).unwrap()
    }

    fn wire(value: &Value) -> SecretBytes {
        bytes(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn the_record_is_bounded_and_rejects_unchecked_credentials_and_identity() {
        let mut padded = VALID.to_vec();
        padded.resize(2048, b' ');
        let account = decode(&bytes(&padded)).unwrap();
        assert_eq!(account.user.id, 7);
        assert_eq!(account.user.login.as_str(), "octocat");
        assert_eq!(
            account.credentials.token.expose().as_str(),
            "ghu_fake_sentinel"
        );
        assert_eq!(
            account.credentials.refresh.expose().as_str(),
            "ghr_fake_sentinel"
        );
        assert_eq!(encode(&account).unwrap().expose().as_slice(), VALID);
        padded.push(b' ');
        assert!(matches!(decode(&bytes(&padded)), Err(VaultError::Bounds)));
        for bad in ["", "{", "null", "[]"] {
            assert!(matches!(
                decode(&bytes(bad.as_bytes())),
                Err(VaultError::Unreadable)
            ));
        }
        for (field, value) in [
            ("id", json!(0)),
            ("id", json!("1")),
            ("login", json!("")),
            ("token", json!("ghu_")),
            ("token", json!("ghp_x")),
            ("token", json!("ghu_x y")),
            ("refresh", json!("ghr_")),
            ("refresh", json!("ghu_x")),
            ("refresh", json!("ghr_x y")),
            ("extra", json!(true)),
        ] {
            let mut bad = valid();
            bad[field] = value;
            assert!(
                matches!(decode(&wire(&bad)), Err(VaultError::Unreadable)),
                "{field}"
            );
        }
        for (field, prefix) in [("token", "ghu_"), ("refresh", "ghr_")] {
            for length in [255, 256] {
                let mut value = valid();
                value[field] = json!(format!("{prefix}{}", "x".repeat(length - 4)));
                value["id"] = json!(u64::MAX);
                match decode(&wire(&value)) {
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

    #[test]
    fn both_credentials_expiries_and_the_spent_marker_roundtrip_without_debug_content() {
        for spent in [false, true] {
            let mut value = valid();
            value["refresh_sent"] = json!(spent);
            let account = decode(&wire(&value)).unwrap();
            let restored = decode(&encode(&account).unwrap()).unwrap();
            assert_eq!(
                restored.credentials.token.expose().as_str(),
                "ghu_fake_sentinel"
            );
            assert_eq!(
                restored.credentials.refresh.expose().as_str(),
                "ghr_fake_sentinel"
            );
            assert_eq!(restored.credentials.expires_at_s, 1_700_028_800);
            assert_eq!(restored.credentials.refresh_expires_at_s, 1_715_897_600);
            assert_eq!(restored.refresh_sent, spent);
            assert_eq!(restored.user.login.as_str(), "octocat");
            assert_eq!(restored.user.id, 7);
            let debug = format!("{restored:?}");
            for marker in ["ghu_fake_sentinel", "ghr_fake_sentinel", "octocat"] {
                assert!(!debug.contains(marker));
                assert!(!debug.contains(&format!("{:?}", marker.as_bytes())));
            }
        }
    }

    #[test]
    fn required_pair_fields_reject_missing_null_duplicate_and_mistyped_values() {
        let legacy = br#"{"token":"ghu_fake_sentinel","login":"octocat","id":7}"#;
        assert!(matches!(
            decode(&bytes(legacy)),
            Err(VaultError::Unreadable)
        ));
        for field in [
            "token",
            "refresh",
            "expires_at_s",
            "refresh_expires_at_s",
            "refresh_sent",
            "login",
            "id",
        ] {
            let mut missing = valid();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                matches!(decode(&wire(&missing)), Err(VaultError::Unreadable)),
                "{field}"
            );
            let mut null = valid();
            null[field] = Value::Null;
            assert!(
                matches!(decode(&wire(&null)), Err(VaultError::Unreadable)),
                "{field}"
            );
            let mut mistyped = valid();
            mistyped[field] = json!([]);
            assert!(
                matches!(decode(&wire(&mistyped)), Err(VaultError::Unreadable)),
                "{field}"
            );
            let mut duplicate = String::from_utf8(VALID.to_vec()).unwrap();
            duplicate.pop();
            write!(duplicate, ",{field:?}:{}}}", valid()[field]).unwrap();
            assert!(
                matches!(
                    decode(&bytes(duplicate.as_bytes())),
                    Err(VaultError::Unreadable)
                ),
                "{field}"
            );
        }
        for field in ["expires_at_s", "refresh_expires_at_s"] {
            for bad in [json!(-1), json!(1.5), json!("28800"), json!(false)] {
                let mut value = valid();
                value[field] = bad;
                assert!(
                    matches!(decode(&wire(&value)), Err(VaultError::Unreadable)),
                    "{field}"
                );
            }
            let overflow = String::from_utf8(VALID.to_vec()).unwrap().replace(
                &format!("\"{field}\":{}", valid()[field]),
                &format!("\"{field}\":18446744073709551616"),
            );
            assert!(matches!(
                decode(&bytes(overflow.as_bytes())),
                Err(VaultError::Unreadable)
            ));
        }
        for bad in [json!(0), json!("false"), json!({})] {
            let mut value = valid();
            value["refresh_sent"] = bad;
            assert!(matches!(decode(&wire(&value)), Err(VaultError::Unreadable)));
        }
    }

    #[test]
    fn positive_expiries_and_bounded_spans_are_checked_without_imposing_order_or_overflow() {
        for (access, refresh) in [
            (0, 1),
            (1, 0),
            (0, 0),
            (1, 31_622_402),
            (86_402, 1),
            (1, u64::MAX),
            (u64::MAX, 1),
        ] {
            let mut value = valid();
            value["expires_at_s"] = json!(access);
            value["refresh_expires_at_s"] = json!(refresh);
            assert!(matches!(decode(&wire(&value)), Err(VaultError::Unreadable)));
            let mut account = decode(&bytes(VALID)).unwrap();
            account.credentials.expires_at_s = access;
            account.credentials.refresh_expires_at_s = refresh;
            assert!(matches!(encode(&account), Err(VaultError::Unreadable)));
        }
        for (access, refresh) in [
            (1, 1),
            (2, 1),
            (1, 2),
            (1, 15_897_601),
            (1, 31_622_401),
            (86_401, 1),
            (u64::MAX - 1, u64::MAX),
            (u64::MAX, u64::MAX - 1),
            (u64::MAX, u64::MAX),
        ] {
            let mut value = valid();
            value["expires_at_s"] = json!(access);
            value["refresh_expires_at_s"] = json!(refresh);
            let account = decode(&wire(&value)).unwrap();
            let restored = decode(&encode(&account).unwrap()).unwrap();
            assert_eq!(restored.credentials.expires_at_s, access);
            assert_eq!(restored.credentials.refresh_expires_at_s, refresh);
        }
        let mut account = decode(&bytes(VALID)).unwrap();
        account.user.id = 0;
        assert!(matches!(encode(&account), Err(VaultError::Unreadable)));
    }
}
