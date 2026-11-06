use super::*;
use kumochi_protocol::constants::{
    account_targets, anthropic_key_target, anthropic_staging_target,
};
use std::collections::BTreeSet;

/// Every name the vault writes or deletes is a write-once generation name: it never touches a
/// legacy name, and everything it wrote but no longer holds current is deleted again.
#[test]
fn every_vault_write_and_delete_uses_generation_names_only() {
    let (_scratch, path, service, store) = setup("vault-inventory");
    let mut vault = open(&store, &service, &path);
    store.arm(usize::MAX, Fault::Refuse);
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
    vault
        .save_anthropic_key(b"sk-ant-api-inventory-fake-key")
        .expect("key");
    vault
        .anthropic(crate::connections::anthropic::KeyOp::Remove)
        .expect("remove fake key");
    let legacy: BTreeSet<_> = account_targets(&service)
        .chain([
            anthropic_key_target(&service),
            anthropic_staging_target(&service),
        ])
        .collect();
    let prefix = format!("{service}/gen/");
    let (written, deleted) = {
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
            assert!(target.starts_with(&prefix), "a generation name: {target}");
            assert!(!legacy.contains(target), "never a legacy name: {target}");
        }
        (written, deleted)
    };
    let current: BTreeSet<_> = current_names(&vault).into_iter().collect();
    assert!(
        current
            .iter()
            .all(|name| name.starts_with(&format!("{prefix}host/"))
                || name.starts_with(&format!("{prefix}chatgpt/"))),
        "only the host and the last account are current"
    );
    assert!(current.is_subset(&written));
    assert_eq!(
        written
            .difference(&current)
            .cloned()
            .collect::<BTreeSet<_>>(),
        deleted,
        "every replaced or removed generation, and nothing current, was deleted"
    );
    assert_only_current(&vault, &store);
}
