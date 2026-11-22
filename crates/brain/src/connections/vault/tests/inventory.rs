use super::*;
use kumochi_protocol::constants::{
    account_targets, anthropic_key_target, anthropic_staging_target,
};
use std::collections::BTreeSet;

/// Every name the vault writes is a write-once generation name, and everything it wrote but no
/// longer holds current is deleted again. The only legacy mutations are the open's deletes of
/// all 262 earlier names, each once.
#[test]
fn every_vault_write_and_delete_uses_generation_names_only() {
    let (_scratch, path, service, store) = setup("vault-inventory");
    let legacy: Vec<_> = account_targets(&service).collect();
    for name in &legacy {
        store
            .set(name, &Secret::new(Zeroizing::new(b"legacy".to_vec())))
            .expect("legacy name");
    }
    store.0.lock().expect("fake lock").mutations.clear();
    let mut vault = open(&store, &service, &path);
    {
        let state = store.0.lock().expect("fake lock");
        let swept: Vec<_> = state.mutations[..legacy.len()]
            .iter()
            .map(|mutation| match mutation {
                Mutation::Delete(target) => target.clone(),
                Mutation::Set(target) => panic!("a legacy write: {target}"),
            })
            .collect();
        assert_eq!(swept, legacy, "the open deletes every legacy name once");
        assert_eq!(state.mutations.len(), legacy.len(), "and nothing else");
    }
    assert_eq!(
        store.names(),
        Vec::<String>::new(),
        "no legacy name is left"
    );
    store.0.lock().expect("fake lock").mutations.clear();
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
        .anthropic(crate::connections::anthropic::KeyOp::Remove(
            vault.anthropic_revision(),
        ))
        .1
        .expect("remove fake key");
    let legacy: BTreeSet<_> = legacy.into_iter().collect();
    assert!(legacy.contains(&anthropic_key_target(&service)));
    assert!(legacy.contains(&anthropic_staging_target(&service)));
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

/// The first clean pass doesn't finish cleanup, but a later clean pass does. Refusal leaves
/// every exact legacy name owed; once finished, only the staging name is still deleted.
#[test]
fn legacy_cleanup_stops_after_a_clean_pass_in_a_later_open() {
    for refused_on in [None, Some(1), Some(2)] {
        let (_scratch, path, service, store) = setup("vault-legacy-done");
        let active = anthropic_key_target(&service);
        let staged = anthropic_staging_target(&service);
        let plant = |name: &String| {
            store
                .set(name, &Secret::new(Zeroizing::new(b"legacy".to_vec())))
                .expect("legacy name");
        };
        if refused_on != Some(1) {
            drop(open(&store, &service, &path));
        }
        let mut legacy: Vec<_> = account_targets(&service).collect();
        legacy.sort();
        for name in &legacy {
            plant(name);
        }
        if refused_on.is_some() {
            store.0.lock().expect("fake lock").refusing = true;
            drop(open(&store, &service, &path));
            assert_eq!(
                store.names(),
                legacy,
                "a refused pass leaves every earlier name owed"
            );
            store.0.lock().expect("fake lock").refusing = false;
        }
        drop(open(&store, &service, &path));
        assert!(
            store.names().is_empty(),
            "the later clean open deletes every earlier name"
        );

        plant(&active);
        plant(&staged);
        store.0.lock().expect("fake lock").mutations.clear();
        drop(open(&store, &service, &path));
        assert_eq!(
            store.names(),
            vec![active],
            "a finished cleanup leaves the rest alone"
        );
        let mutations = store.0.lock().expect("fake lock").mutations.clone();
        assert!(matches!(mutations.as_slice(), [Mutation::Delete(name)] if *name == staged));
    }
}
