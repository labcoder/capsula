use super::*;
use crate::connections::records::{
    MAX_EMAIL_BYTES, MAX_EXCHANGES, MAX_ID_BYTES, MAX_REFRESH_BYTES, MAX_SCOPE_BYTES,
    MAX_TOKEN_BYTES,
};

#[test]
fn every_oversized_account_field_is_refused_before_any_write() {
    for field in 0..7 {
        let (_scratch, path, service, store) = setup("account-field-bound");
        let mut vault = open(&store, &service, &path);
        let mut record = account("one").expose().clone();
        match field {
            0 => record.client_id = Zeroizing::new("x".repeat(MAX_ID_BYTES + 1)),
            1 => record.subject = Some(Zeroizing::new("x".repeat(MAX_ID_BYTES + 1))),
            2 => record.email = Some(Zeroizing::new("x".repeat(MAX_EMAIL_BYTES + 1))),
            field => {
                let tokens = record.tokens.as_mut().expect("test tokens");
                match field {
                    3 => tokens.access_token = Zeroizing::new("x".repeat(MAX_TOKEN_BYTES + 1)),
                    4 => tokens.refresh_token = Zeroizing::new("x".repeat(MAX_REFRESH_BYTES + 1)),
                    5 => tokens.id_token = Some(Zeroizing::new("x".repeat(MAX_TOKEN_BYTES + 1))),
                    6 => tokens.scope = Zeroizing::new("x".repeat(MAX_SCOPE_BYTES + 1)),
                    _ => unreachable!("seven account fields"),
                }
            }
        }
        assert!(
            matches!(
                vault.save_account(&Secret::new(record)),
                SaveResult::NotCommitted(VaultError::Bounds)
            ),
            "one byte over the field bound fails"
        );
        assert!(
            store.names().is_empty(),
            "no marker, chunk or manifest was written"
        );
    }
}

#[test]
fn field_limits_are_bytes_and_exact_limits_are_accepted() {
    let (_scratch, path, service, store) = setup("account-exact-bounds");
    let mut vault = open(&store, &service, &path);
    let mut record = account("one").expose().clone();
    record.client_id = Zeroizing::new("x".repeat(MAX_ID_BYTES));
    record.subject = Some(Zeroizing::new("x".repeat(MAX_ID_BYTES)));
    record.email = Some(Zeroizing::new("x".repeat(MAX_EMAIL_BYTES)));
    let tokens = record.tokens.as_mut().expect("test tokens");
    tokens.access_token = Zeroizing::new("x".repeat(MAX_TOKEN_BYTES));
    tokens.refresh_token = Zeroizing::new("x".repeat(MAX_REFRESH_BYTES));
    tokens.id_token = Some(Zeroizing::new("x".repeat(MAX_TOKEN_BYTES)));
    tokens.scope = Zeroizing::new("x".repeat(MAX_SCOPE_BYTES));
    let record = Secret::new(record);
    committed(vault.save_account(&record));
    assert!(
        vault
            .load_account()
            .expect("read")
            .expect("record")
            .expose()
            == record.expose(),
        "exact byte bounds round trip"
    );
    let mut oversized = record.expose().clone();
    oversized.client_id = Zeroizing::new("é".repeat(MAX_ID_BYTES / 2 + 1));
    assert!(
        matches!(
            vault.save_account(&Secret::new(oversized)),
            SaveResult::NotCommitted(VaultError::Bounds)
        ),
        "UTF-8 byte length, not character count"
    );
}

#[test]
fn serialized_escaping_cannot_grow_a_record_past_48_kib() {
    let (_scratch, path, service, store) = setup("account-json-bound");
    let mut vault = open(&store, &service, &path);
    let mut record = account("one").expose().clone();
    record.tokens.as_mut().expect("test tokens").access_token =
        Zeroizing::new("\0".repeat(MAX_TOKEN_BYTES));
    assert!(
        matches!(
            vault.save_account(&Secret::new(record)),
            SaveResult::NotCommitted(VaultError::Bounds)
        ),
        "JSON escaping counts toward the serialized bound"
    );
    assert!(
        store.names().is_empty(),
        "serialization fails before storage"
    );
    let mut buffer = JsonBuffer {
        bytes: Zeroizing::new(Vec::with_capacity(3)),
        limit: 3,
    };
    buffer.write_all(b"abc").expect("exact buffer limit");
    assert!(buffer.write_all(b"d").is_err(), "no growth past the limit");
    assert_eq!(buffer.bytes.as_slice(), b"abc");
    assert_eq!(format!("{buffer:?}"), "JsonBuffer(***)");
}

#[test]
fn revocation_count_field_and_serialized_bounds_precede_marker_writes() {
    for case in 0..5 {
        let (_scratch, path, service, store) = setup("revoke-bound");
        let mut vault = open(&store, &service, &path);
        let mut record = list("one").expose().clone();
        match case {
            0 => record.entries[0].client_id = Zeroizing::new("x".repeat(MAX_ID_BYTES + 1)),
            1 => {
                record.entries[0].refresh_token = Zeroizing::new("x".repeat(MAX_REFRESH_BYTES + 1));
            }
            2 => record.entries = vec![record.entries[0].clone(); MAX_EXCHANGES + 1],
            3 => {
                record.entries[0].origin = Origin::Account;
                record.entries.push(record.entries[0].clone());
            }
            4 => record.entries[0].refresh_token = Zeroizing::new("\0".repeat(MAX_REFRESH_BYTES)),
            _ => unreachable!("five revocation bounds"),
        }
        assert!(
            matches!(
                vault.save_revocations(&Secret::new(record)),
                SaveResult::NotCommitted(VaultError::Bounds)
            ),
            "invalid list rejected"
        );
        assert!(
            store.names().is_empty(),
            "no sweep marker for a value that can never fit"
        );
    }
    let (_scratch, path, service, store) = setup("revoke-max-capacity");
    let mut vault = open(&store, &service, &path);
    let mut entry = list("one").expose().entries[0].clone();
    entry.client_id = Zeroizing::new("x".repeat(MAX_ID_BYTES));
    entry.refresh_token = Zeroizing::new("x".repeat(MAX_REFRESH_BYTES));
    let mut entries = vec![entry.clone(); MAX_EXCHANGES];
    entry.origin = Origin::Account;
    entries.push(entry);
    let record = Secret::new(RevocationList { entries });
    committed(vault.save_revocations(&record));
    assert!(
        vault
            .load_revocations()
            .expect("read")
            .expect("exists")
            .expose()
            == record.expose(),
        "four exchanges and one account fit"
    );
}

#[test]
fn malformed_manifests_and_partial_records_never_fall_back_to_an_old_slot() {
    let (_scratch, path, service, store) = setup("malformed-vault");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    for manifest in [
        r#"{"slot":2,"chunks":1}"#,
        r#"{"slot":0,"chunks":0}"#,
        r#"{"slot":0,"chunks":26}"#,
        r#"{"slot":0,"chunks":18446744073709551615}"#,
    ] {
        store
            .set(
                &format!("{service}/chatgpt"),
                &Secret::new(Zeroizing::new(manifest.as_bytes().to_vec())),
            )
            .expect("corrupt test manifest");
        assert!(
            matches!(vault.load_account(), Err(VaultError::Unreadable)),
            "manifest is validated before loading chunks"
        );
    }
    store
        .set(
            &format!("{service}/chatgpt"),
            &Secret::new(Zeroizing::new(br#"{"slot":1,"chunks":1}"#.to_vec())),
        )
        .expect("missing active chunk");
    assert!(
        matches!(
            vault.load_account(),
            Err(VaultError::Store(CredentialStoreError::MissingChunk))
        ),
        "no old-slot fallback"
    );
    store
        .set(
            &format!("{service}/chatgpt/1/0"),
            &Secret::new(Zeroizing::new(b"secret-test-sentinel-not-json".to_vec())),
        )
        .expect("bad JSON");
    let error = vault.load_account().expect_err("malformed account");
    assert_eq!(format!("{error:?}"), "Unreadable");
    assert!(
        !format!("{error}").contains("secret-test-sentinel"),
        "serde diagnostics never escape"
    );
}

#[test]
fn every_record_debug_is_redacted() {
    let (_scratch, path, service, store) = setup("vault-debug");
    let mut vault = open(&store, &service, &path);
    let account = account("secret-test-sentinel");
    let list = list("sentinel");
    assert_eq!(format!("{vault:?}"), "Vault(***)");
    assert_eq!(format!("{:?}", account.expose()), "Account(***)");
    assert_eq!(
        format!("{:?}", account.expose().tokens),
        "Some(Tokens(***))"
    );
    assert_eq!(format!("{:?}", list.expose()), "RevocationList(***)");
    assert_eq!(format!("{:?}", list.expose().entries[0]), "Revocable(***)");
    assert_eq!(format!("{:?}", vault.host_id().expect("host")), "***");
    assert_eq!(
        format!("{:?}", vault.marker().expect("marker")),
        "SignOutMarker(***)"
    );
    let pending = SignOut {
        disposition: Disposition::Keep,
        phase: Phase::Revoking,
    };
    assert_eq!(format!("{pending:?}"), "SignOut(***)");
}
