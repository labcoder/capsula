use super::*;
use crate::connections::anthropic::{KeyError, KeyOp};
use kumochi_protocol::constants::{ANTHROPIC_KEY_ACCOUNT, ANTHROPIC_STAGING_ACCOUNT};
use kumochi_protocol::message::KeyRejection;

const KEY: &str = "sk-ant-api-fake-sentinel-never-real";

#[test]
fn adoption_uses_keyring_names_and_cleans_staging_on_all_outcomes() {
    for (label, bytes, expected) in [
        ("valid", KEY.as_bytes(), None),
        (
            "subscription",
            b"sk-ant-oat-fake-sentinel".as_slice(),
            Some(KeyRejection::SubscriptionToken),
        ),
        (
            "admin",
            b"sk-ant-admin-fake-sentinel".as_slice(),
            Some(KeyRejection::AdminKey),
        ),
        ("utf8", b"\xff".as_slice(), Some(KeyRejection::Format)),
    ] {
        let (_scratch, path, service, store) = setup(label);
        let staged = format!("{ANTHROPIC_STAGING_ACCOUNT}.{service}");
        let active = format!("{ANTHROPIC_KEY_ACCOUNT}.{service}");
        store
            .set(&staged, &Secret::new(Zeroizing::new(bytes.to_vec())))
            .expect("stage fake");
        let mut vault = open(&store, &service, &path);
        let result = vault.anthropic(KeyOp::Load);
        match expected {
            None => assert_eq!(
                result.expect("adopted").expect("key").expose().as_str(),
                KEY
            ),
            Some(expected) => assert!(
                matches!(result, Err(KeyError::Rejected(reason)) if reason == expected),
                "exact rejection"
            ),
        }
        assert_eq!(store.get(&staged).expect("read fake"), None);
        assert_eq!(
            store.names(),
            if expected.is_none() {
                vec![active]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn missing_or_unreadable_staging_is_cleaned_and_never_adopted() {
    let (_scratch, path, service, store) = setup("staging-read");
    let staged = format!("{ANTHROPIC_STAGING_ACCOUNT}.{service}");
    let mut vault = open(&store, &service, &path);
    assert!(
        matches!(
            vault.anthropic(KeyOp::Adopt),
            Err(KeyError::Rejected(KeyRejection::Format))
        ),
        "missing paste rejected"
    );
    store
        .set(
            &staged,
            &Secret::new(Zeroizing::new(KEY.as_bytes().to_vec())),
        )
        .expect("fake paste");
    store.0.lock().expect("fake lock").denied_read = Some(staged.clone());
    assert!(
        matches!(vault.anthropic(KeyOp::Adopt), Err(KeyError::Vault(_))),
        "failed read not adopted"
    );
    store.0.lock().expect("fake lock").denied_read = None;
    assert_eq!(store.get(&staged).expect("fake read"), None);
    assert_eq!(store.names(), Vec::<String>::new());
}

#[test]
fn adoption_failures_preserve_the_old_key_and_report_cleanup_failure() {
    let (_scratch, path, service, store) = setup("staging-write");
    let staged = format!("{ANTHROPIC_STAGING_ACCOUNT}.{service}");
    let active = format!("{ANTHROPIC_KEY_ACCOUNT}.{service}");
    let old = Secret::new(Zeroizing::new(b"sk-ant-api-old-fake-value".to_vec()));
    store.set(&active, &old).expect("old fake");
    store
        .set(
            &staged,
            &Secret::new(Zeroizing::new(KEY.as_bytes().to_vec())),
        )
        .expect("fake paste");
    let mut vault = open(&store, &service, &path);
    store.arm(0, Fault::Refuse);
    assert!(
        matches!(vault.anthropic(KeyOp::Adopt), Err(KeyError::Vault(_))),
        "write refused"
    );
    assert_eq!(store.get(&active).expect("old fake"), Some(old));
    assert_eq!(store.get(&staged).expect("staging gone"), None);
    store
        .set(
            &staged,
            &Secret::new(Zeroizing::new(KEY.as_bytes().to_vec())),
        )
        .expect("fake paste");
    store.arm(1, Fault::Refuse);
    assert!(
        matches!(vault.anthropic(KeyOp::Adopt), Err(KeyError::Vault(_))),
        "cleanup refusal is not success"
    );
    assert_eq!(
        store.get(&staged).expect("staged retained"),
        Some(Secret::new(Zeroizing::new(KEY.as_bytes().to_vec())))
    );
}

#[test]
fn removal_attempts_both_names_and_never_claims_partial_success() {
    for failing in [0, 1] {
        let (_scratch, path, service, store) = setup("key-remove");
        let staged = format!("{ANTHROPIC_STAGING_ACCOUNT}.{service}");
        let active = format!("{ANTHROPIC_KEY_ACCOUNT}.{service}");
        for target in [&active, &staged] {
            store
                .set(
                    target,
                    &Secret::new(Zeroizing::new(KEY.as_bytes().to_vec())),
                )
                .expect("fake key");
        }
        let mut vault = open(&store, &service, &path);
        store.arm(failing, Fault::Refuse);
        assert!(
            matches!(vault.anthropic(KeyOp::Remove), Err(KeyError::Vault(_))),
            "partial delete refused"
        );
        assert_eq!(
            store.names(),
            vec![if failing == 0 { active } else { staged }]
        );
        assert_eq!(
            vault
                .anthropic(KeyOp::Remove)
                .expect("retry deletes")
                .map(|_| ()),
            None
        );
        assert_eq!(store.names(), Vec::<String>::new());
    }
}
