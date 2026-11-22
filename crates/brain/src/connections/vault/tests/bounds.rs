use super::*;
use crate::connections::records::{
    MAX_EMAIL_BYTES, MAX_EXCHANGES, MAX_ID_BYTES, MAX_REFRESH_BYTES, MAX_SCOPE_BYTES,
    MAX_TOKEN_BYTES, Record, entry_fits,
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

/// The 48 KiB serialized bound on an account, at exactly 49,152 bytes and one over. The JSON
/// around the 32,768 escaped access-token bytes and the ID token is 179 bytes, counted by hand.
#[test]
fn an_account_of_exactly_48_kib_serialized_is_saved_and_one_byte_more_is_refused() {
    let sized = |id_token: usize| Account {
        client_id: Zeroizing::new("c".to_owned()),
        subject: None,
        email: None,
        plan_enabled: false,
        tokens: Some(Tokens {
            access_token: Zeroizing::new("\\".repeat(MAX_TOKEN_BYTES)),
            refresh_token: Zeroizing::new("r".to_owned()),
            id_token: Some(Zeroizing::new("x".repeat(id_token))),
            scope: Zeroizing::new("s".to_owned()),
            expires_at: 0,
            pending_identity_validation: false,
            refresh_sent: false,
            connection: 0,
        }),
    };
    let (exact, over) = (sized(16_205), sized(16_206));
    assert_eq!(serde_json::to_vec(&exact).expect("JSON").len(), 49_152);
    assert_eq!(serde_json::to_vec(&over).expect("JSON").len(), 49_153);
    assert!(matches!(exact.validate(), Ok(())), "exactly the bound");
    assert!(matches!(over.validate(), Err(VaultError::Bounds)));
    let (_scratch, path, service, store) = setup("account-exact-bound");
    let mut vault = open(&store, &service, &path);
    assert!(matches!(
        vault.save_account(&Secret::new(over)),
        SaveResult::NotCommitted(VaultError::Bounds)
    ));
    assert!(store.names().is_empty(), "refused before any write");
    let mut escaped = account("one").expose().clone();
    escaped.tokens.as_mut().expect("test tokens").access_token =
        Zeroizing::new("\0".repeat(MAX_TOKEN_BYTES));
    assert!(
        matches!(
            vault.save_account(&Secret::new(escaped)),
            SaveResult::NotCommitted(VaultError::Bounds)
        ),
        "NUL escapes count toward the serialized bound"
    );
    assert!(store.names().is_empty(), "escaping fails before storage");
    let exact = Secret::new(exact);
    committed(vault.save_account(&exact));
    assert_eq!(
        current_names(&vault).len(),
        25,
        "a full record spans 25 chunks"
    );
    assert!(
        vault.load_account().expect("read").expect("saved").expose() == exact.expose(),
        "all 25 chunks read back whole"
    );
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

/// CAPS-116: entries at the per-entry bound always fit together in one list record.
#[test]
fn a_full_list_of_entries_at_the_entry_bound_is_saved() {
    let (_scratch, path, service, store) = setup("revocation-entry-bound");
    let mut vault = open(&store, &service, &path);
    let client_id = Zeroizing::new("x".repeat(MAX_ID_BYTES));
    let mut refresh_token = Zeroizing::new("\0".repeat(MAX_REFRESH_BYTES));
    while !entry_fits(&client_id, &refresh_token) {
        refresh_token.pop();
    }
    let entry = Revocable {
        client_id,
        refresh_token,
        added_at: u64::MAX,
        origin: Origin::Exchange,
    };
    let mut entries = vec![entry.clone(); MAX_EXCHANGES];
    entries.push(Revocable {
        origin: Origin::Account,
        ..entry
    });
    committed(vault.save_revocations(&Secret::new(RevocationList { entries })));
}

/// A damaged current generation is an error, even with an older generation's bytes back.
#[test]
fn a_damaged_current_generation_never_falls_back_to_an_older_one() {
    let (_scratch, path, service, store) = setup("malformed-vault");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    let old = store.snapshot();
    committed(vault.save_account(&account("new")));
    store.rollback(&old, &old.keys().cloned().collect::<Vec<_>>());
    let names = current_names(&vault);
    let first = names[0].clone();
    let clean = store.snapshot();
    let mut damaged = store.snapshot();
    let length = damaged[&first].expose().len();
    let sentinel = b"secret-test-sentinel-not-json"
        .iter()
        .copied()
        .cycle()
        .take(length)
        .collect();
    damaged.insert(first.clone(), Secret::new(Zeroizing::new(sentinel)));
    store.rollback(&damaged, std::slice::from_ref(&first));
    let error = vault.load_account().expect_err("malformed account");
    assert_eq!(format!("{error:?}"), "Unreadable");
    assert!(
        !format!("{error}").contains("secret-test-sentinel"),
        "serde diagnostics never escape"
    );
    let mut short = store.snapshot();
    short.insert(
        first.clone(),
        Secret::new(Zeroizing::new(vec![b'{'; length - 1])),
    );
    store.rollback(&short, std::slice::from_ref(&first));
    assert!(
        matches!(
            vault.load_account(),
            Err(VaultError::Store(CredentialStoreError::Unsettled))
        ),
        "a chunk one byte short is unsettled"
    );
    store.rollback(&clean, &names);
    assert_eq!(
        vault
            .load_account()
            .expect("a clean current generation")
            .expect("the account")
            .expose()
            .email
            .as_deref()
            .map(String::as_str),
        Some("new@example.invalid"),
        "the restored generation reads, so only the missing chunk fails next"
    );
    let last = names.last().expect("a chunk").clone();
    store.rollback(&BTreeMap::new(), std::slice::from_ref(&last));
    assert!(
        matches!(
            vault.load_account(),
            Err(VaultError::Store(CredentialStoreError::Unsettled))
        ),
        "a missing chunk is unsettled, with no older generation to fall back to"
    );
}

#[test]
fn every_record_debug_is_redacted() {
    let (_scratch, path, service, store) = setup("vault-debug");
    let mut vault = open(&store, &service, &path);
    let marker = "secret-test-sentinel";
    let account = account(marker);
    let list = list(marker);
    let host = vault.host_id().expect("host");
    let head = vault.authority.current(Kind::Host).expect("head");
    let generation = head.active.expect("saved host").generation.hex();
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Keep))
        .expect("sign-out pending");
    let pending = vault.marker().expect("marker");
    let buffer = JsonBuffer {
        bytes: Zeroizing::new(marker.as_bytes().to_vec()),
        limit: marker.len(),
    };
    for shown in [
        format!("{buffer:?}"),
        format!("{vault:?}"),
        format!("{:?}", account.expose()),
        format!("{:?}", account.expose().tokens),
        format!("{:?}", list.expose()),
        format!("{:?}", list.expose().entries[0]),
        format!("{host:?}"),
        format!("{pending:?}"),
        format!("{:?}", pending.sign_out.expect("pending sign-out")),
    ] {
        for secret in [
            marker,
            "vault-debug",
            service.as_str(),
            generation.as_str(),
            host.expose().as_str(),
            "urn:uuid:",
        ] {
            assert!(!shown.contains(secret));
            let bytes = format!("{:?}", secret.as_bytes());
            assert!(!shown.contains(bytes.trim_matches(['[', ']'])));
        }
    }
}
