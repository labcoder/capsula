use super::*;
use crate::connections::anthropic::{KeyError, KeyOp};
use kumochi_protocol::message::KeyRejection;

const KEY: &str = "sk-ant-api-fake-sentinel-never-real";

fn loaded(vault: &mut Vault<FakeStore>) -> Option<String> {
    vault
        .anthropic(KeyOp::Load)
        .1
        .expect("key read")
        .map(|key| key.expose().as_str().to_owned())
}

#[test]
fn the_current_key_loads_and_each_bad_key_is_rejected_exactly() {
    for (label, value, expected) in [
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
        let mut vault = open(&store, &service, &path);
        vault.save_anthropic_key(value).expect("current key");
        let result = vault.anthropic(KeyOp::Load).1;
        match expected {
            None => assert_eq!(result.expect("loaded").expect("key").expose().as_str(), KEY),
            Some(expected) => assert!(
                matches!(result, Err(KeyError::Rejected(reason)) if reason == expected),
                "exact rejection"
            ),
        }
        assert!(
            is_current(&vault, Kind::AnthropicKey),
            "a load never removes"
        );
    }
}

#[test]
fn removal_commits_its_fence_even_while_every_delete_is_refused() {
    let (_scratch, path, service, store) = setup("key-remove-refused");
    let mut vault = open(&store, &service, &path);
    vault
        .save_anthropic_key(KEY.as_bytes())
        .expect("current key");
    let names = store.names();
    store.0.lock().expect("fake lock").refusing = true;
    assert_eq!(
        vault
            .anthropic(KeyOp::Remove(vault.anthropic_revision()))
            .1
            .expect("the fence commits")
            .map(|_| ()),
        None
    );
    assert_eq!(store.names(), names, "every delete was refused");
    assert_eq!(
        loaded(&mut vault),
        None,
        "the removed key is never read again"
    );
    store.0.lock().expect("fake lock").refusing = false;
    drop(vault);
    let mut vault = open(&store, &service, &path);
    assert!(store.names().is_empty(), "the next open swept the names");
    assert_eq!(loaded(&mut vault), None);
}

#[test]
fn a_missed_read_of_the_current_key_is_unsettled_never_missing() {
    let (_scratch, path, service, store) = setup("key-missed");
    let mut vault = open(&store, &service, &path);
    vault
        .save_anthropic_key(KEY.as_bytes())
        .expect("current key");
    store.0.lock().expect("fake lock").missed_reads = 1;
    assert!(
        matches!(
            vault.anthropic(KeyOp::Load).1,
            Err(KeyError::Vault(VaultError::Store(
                CredentialStoreError::Unsettled
            )))
        ),
        "a miss is an error, not a missing key"
    );
    assert_eq!(loaded(&mut vault).as_deref(), Some(KEY));
}
