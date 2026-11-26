use super::*;

mod exchange;

fn account_chunk(vault: &Vault<FakeStore>, index: usize) -> String {
    let active = vault
        .authority
        .current(Kind::Account)
        .unwrap()
        .active
        .unwrap();
    assert!(index < active.layout.chunks());
    generation_target(
        &vault.service,
        Kind::Account.label(),
        &active.generation.hex(),
        index,
    )
}

fn schema(path: &Path) -> i64 {
    raw(path)
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn admission_requires_an_exact_missing_registration_and_existing_valid_host() {
    use super::super::repair::RepairRefusal;

    let (_scratch, path, service, store) = setup("repair-refusal");
    let mut vault = open(&store, &service, &path);
    assert!(matches!(
        vault.missing_repair_head(0),
        Err(RepairRefusal::Changed)
    ));
    committed(vault.save_account(&account("old")));
    let old = vault.authority.current(Kind::Account).unwrap();
    assert!(matches!(
        vault.missing_repair_head(old.revision),
        Err(RepairRefusal::Present)
    ));

    let first = account_chunk(&vault, 0);
    let next = account_chunk(&vault, 1);
    let snapshot = store.snapshot();
    store.delete(&first).unwrap();
    store.0.lock().unwrap().denied_read = Some(next.clone());
    assert!(matches!(
        vault.missing_repair_head(old.revision),
        Err(RepairRefusal::Vault(VaultError::Store(
            CredentialStoreError::Read(5)
        )))
    ));
    store.0.lock().unwrap().denied_read = None;
    store
        .set(&next, &Secret::new(Zeroizing::new(vec![0])))
        .unwrap();
    assert!(matches!(
        vault.missing_repair_head(old.revision),
        Err(RepairRefusal::Vault(VaultError::Store(
            CredentialStoreError::Unsettled
        )))
    ));
    store.rollback(&snapshot, &[next]);
    assert_eq!(vault.missing_repair_head(old.revision).unwrap(), old);
    let refuse_claim = || panic!("a refused registration must not claim the attempt");
    assert!(matches!(
        vault.confirm_repair(old, refuse_claim),
        Err(RepairRefusal::Vault(VaultError::Fenced))
    ));
    vault
        .write(Kind::Host, &encode(&"not-a-host", 128).unwrap(), None)
        .unwrap();
    assert!(matches!(
        vault.confirm_repair(old, refuse_claim),
        Err(RepairRefusal::Vault(VaultError::Unreadable))
    ));

    let host_value = "urn:uuid:cab1e51c-b69a-49de-8a96-4a6a4fdbc6fa";
    vault
        .write(Kind::Host, &encode(&host_value, 128).unwrap(), None)
        .unwrap();
    vault
        .write(
            Kind::Revocations,
            &encode(list("x").expose(), MAX_RECORD_BYTES).unwrap(),
            None,
        )
        .unwrap();
    assert_eq!(vault.missing_repair_head(old.revision).unwrap(), old);
    assert!(matches!(
        vault.confirm_repair(old, refuse_claim),
        Err(RepairRefusal::Vault(VaultError::Fenced))
    ));
    vault.update_marker(MarkerUpdate::RequestStarted).unwrap();
    assert!(matches!(
        vault.missing_repair_head(old.revision),
        Err(RepairRefusal::Vault(VaultError::Fenced))
    ));
    vault
        .update_marker(MarkerUpdate::RequestFinished {
            outcome_unknown: false,
        })
        .unwrap();

    committed(vault.save_account(&account("other")));
    assert!(matches!(
        vault.confirm_repair(old, refuse_claim),
        Err(RepairRefusal::Changed)
    ));
    assert_eq!(schema(&path), 3);
    assert_eq!(
        vault.load_account().unwrap().unwrap().expose(),
        account("other").expose()
    );
}

#[test]
fn cancelling_only_the_exact_attempt_preserves_the_registration_and_fence() {
    use super::super::repair::RepairRefusal;

    let (_scratch, path, service, store) = setup("repair-exact-cancel");
    let mut vault = open(&store, &service, &path);
    vault.host_id().unwrap();
    committed(vault.save_account(&account("old")));
    vault.save_anthropic_key(b"other-provider").unwrap();
    let first = account_chunk(&vault, 0);
    let snapshot = store.snapshot();
    store.delete(&first).unwrap();
    let revision = vault.authority.current(Kind::Account).unwrap().revision;
    let old = vault.missing_repair_head(revision).unwrap();

    store.rollback(&snapshot, std::slice::from_ref(&first));
    assert!(matches!(
        vault.confirm_repair(old, || true),
        Err(RepairRefusal::Present)
    ));
    store.delete(&first).unwrap();
    let names = store.names();
    store.0.lock().unwrap().mutations.clear();
    assert!(matches!(
        vault.confirm_repair(old, || false),
        Err(RepairRefusal::Changed)
    ));
    assert_eq!(schema(&path), 3);

    let attempt = vault.confirm_repair(old, || true).unwrap();
    assert_eq!(schema(&path), 4);
    assert!(vault.confirm_repair(old, || true).is_err());
    vault.cancel_repair_admission(&attempt).unwrap();
    let retry = vault.confirm_repair(old, || true).unwrap();
    assert!(matches!(
        vault.cancel_repair_admission(&attempt),
        Err(VaultError::Authority)
    ));
    assert!(vault.confirm_repair(old, || true).is_err());
    vault.cancel_repair_admission(&retry).unwrap();

    store.rollback(&snapshot, std::slice::from_ref(&first));
    assert!(matches!(vault.load_account(), Err(VaultError::Authority)));
    assert!(matches!(
        vault.saved_sign_in(None),
        Err(SavedSignInFailure::Unavailable)
    ));
    assert!(matches!(
        vault.save_account(&account("reset")),
        SaveResult::NotCommitted(VaultError::Authority)
    ));
    assert!(matches!(
        vault.remove(&[Kind::Account], None),
        Err(VaultError::Authority)
    ));
    assert_eq!(
        vault
            .read_bytes(Kind::AnthropicKey)
            .unwrap()
            .unwrap()
            .expose()
            .as_slice(),
        b"other-provider"
    );
    assert!(store.0.lock().unwrap().mutations.is_empty());
    let mut expected_names = names;
    expected_names.push(first);
    expected_names.sort();
    assert_eq!(store.names(), expected_names);

    drop(vault);
    let conn = raw(&path);
    let row: (String, bool, bool, i64) = conn.query_row(
        "SELECT phase, attempt IS NULL, fenced=generations.generation, heads.revision
         FROM chatgpt_repair JOIN generations ON generations.kind='chatgpt' AND generations.state='active'
         JOIN heads ON heads.kind=generations.kind WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(
        row,
        ("idle".into(), true, true, i64::try_from(revision).unwrap())
    );
    assert!(matches!(
        Vault::with_service(store, &service, &path),
        Err(VaultError::Authority)
    ));
}

#[test]
fn admission_refuses_a_pending_revocation_even_without_an_active_list_or_marker() {
    use super::super::repair::RepairRefusal;

    let (_scratch, path, service, store) = setup("repair-pending-revocation");
    let mut vault = open(&store, &service, &path);
    vault.host_id().unwrap();
    committed(vault.save_account(&account("old")));
    store.delete(&account_chunk(&vault, 0)).unwrap();
    let current = vault.authority.current(Kind::Account).unwrap();
    let old = vault.missing_repair_head(current.revision).unwrap();

    let revocations = vault.authority.current(Kind::Revocations).unwrap();
    let bytes = encode(list("x").expose(), MAX_RECORD_BYTES).unwrap();
    let plan = vault
        .authority
        .begin(
            Kind::Revocations,
            revocations.revision,
            bytes.expose().len(),
        )
        .unwrap();
    let _unactivated = generation::write(&store, &service, plan, &bytes).unwrap();
    assert!(vault.load_revocations().unwrap().is_none());
    assert_eq!(vault.marker().unwrap(), SignOutMarker::default());
    let names = store.names();
    store.0.lock().unwrap().mutations.clear();

    assert!(matches!(
        vault.confirm_repair(old, || true),
        Err(RepairRefusal::Vault(VaultError::Authority))
    ));
    assert_eq!(schema(&path), 3);
    assert_eq!(vault.authority.current(Kind::Account).unwrap(), old);
    assert_eq!(
        vault.authority.current(Kind::Revocations).unwrap(),
        revocations
    );
    assert_eq!(store.names(), names);
    assert!(store.0.lock().unwrap().mutations.is_empty());
}

#[test]
fn sql_refusal_neither_spends_admission_nor_reports_a_cancel() {
    let (_scratch, path, service, store) = setup("repair-sql-refusal");
    let mut vault = open(&store, &service, &path);
    vault.host_id().unwrap();
    committed(vault.save_account(&account("old")));
    store.delete(&account_chunk(&vault, 0)).unwrap();
    let old = vault.missing_repair_head(1).unwrap();
    let conn = raw(&path);

    conn.execute_batch("CREATE TABLE generations_v3 (sentinel INTEGER)")
        .unwrap();
    assert!(vault.confirm_repair(old, || true).is_err());
    assert_eq!(schema(&path), 3);
    assert_eq!(vault.authority.current(Kind::Account).unwrap(), old);
    conn.execute_batch("DROP TABLE generations_v3").unwrap();

    let attempt = vault.confirm_repair(old, || true).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_cancel BEFORE UPDATE OF phase ON chatgpt_repair
         WHEN NEW.phase='idle' BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    assert!(matches!(
        vault.cancel_repair_admission(&attempt),
        Err(VaultError::Authority)
    ));
    assert!(vault.confirm_repair(old, || true).is_err());
    conn.execute_batch("DROP TRIGGER refuse_cancel").unwrap();
    vault.cancel_repair_admission(&attempt).unwrap();
}
