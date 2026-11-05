use super::*;
use kumochi_protocol::constants::account_targets;
use std::collections::BTreeSet;

#[test]
fn inventory_covers_every_vault_write_and_delete_target() {
    let (_scratch, path, service, store) = setup("vault-inventory");
    let mut vault = open(&store, &service, &path);
    vault.host_id().expect("host");
    committed(vault.save_account(&account("old")));
    committed(vault.save_account(&account("new")));
    committed(vault.save_revocations(&list("old")));
    committed(vault.save_revocations(&list("new")));
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("sign-out");
    vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: false,
        })
        .expect("checkpoint");
    gone(vault.delete_for_sign_out());
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("finish");
    committed(vault.save_account(&account("again")));
    vault
        .update_marker(MarkerUpdate::SignInCommitted)
        .expect("clear sign-out notice");
    vault.erase(Chunked::Account).expect("erase account");

    for record in [Chunked::Account, Chunked::Revoke] {
        let bytes = Secret::new(Zeroizing::new(vec![b'x'; MAX_RECORD_BYTES]));
        assert!(!vault.save(record, &bytes).expect("full slot"));
        assert!(!vault.save(record, &bytes).expect("rotate full slot"));
        vault.erase(record).expect("erase both slots");
        let bytes = Secret::new(Zeroizing::new(vec![b'x'; CHUNK_SIZE * MAX_CHUNKS]));
        assert!(!vault.save(record, &bytes).expect("chunk storage bound"));
        vault.erase(record).expect("erase storage-bound slot");
    }
    let inventory: BTreeSet<_> = account_targets(&service).collect();
    let state = store.0.lock().expect("fake lock");
    let mut written = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    for mutation in &state.mutations {
        let target = match mutation {
            Mutation::Set(target) => {
                written.insert(target.clone());
                target
            }
            Mutation::Delete(target) => {
                deleted.insert(target.clone());
                target
            }
        };
        assert!(
            inventory.contains(target),
            "target outside the inventory: {target}"
        );
    }
    assert_eq!(written.len(), 182);
    assert_eq!(deleted.len(), 259);
    assert!(written.contains(&format!("{service}/chatgpt/0/63")));
    assert!(written.contains(&format!("{service}/revoke/1/24")));
    assert!(!deleted.contains(&format!("{service}/host")));
}
