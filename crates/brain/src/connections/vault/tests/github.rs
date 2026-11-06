use super::*;
use crate::connections::github::{
    device::{Removed, Stamp},
    lane::{Job, LoadError, Reply},
};

fn bytes(value: &[u8]) -> SecretBytes {
    Secret::new(Zeroizing::new(value.to_vec()))
}

const VALID: &[u8] = br#"{"token":"ghu_fake_test_only_token","login":"octocat","id":7}"#;

#[test]
fn cleanup_retries_in_the_same_process_and_requires_a_later_open_to_sweep() {
    let (_scratch, path, service, store) = setup("github-retry");
    let mut vault = open(&store, &service, &path);
    vault.write(Kind::GitHub, &bytes(VALID), None).unwrap();
    store.0.lock().unwrap().refusing = true;
    assert!(matches!(
        vault.github(Job::Remove { generation: 1 }),
        Reply::Removed {
            generation: 1,
            result: Removed::Refused
        }
    ));
    assert_eq!(vault.authority.current(Kind::GitHub).unwrap().active, None);
    assert!(!store.names().is_empty());
    store.0.lock().unwrap().refusing = false;
    assert!(matches!(
        vault.github(Job::Remove { generation: 2 }),
        Reply::Removed {
            generation: 2,
            result: Removed::Clean
        }
    ));
    assert_eq!(store.names(), Vec::<String>::new());
    assert_eq!(vault.authority.retired_kind(Kind::GitHub).unwrap().len(), 1);
    drop(vault);
    let vault = open(&store, &service, &path);
    assert_eq!(vault.authority.retired_kind(Kind::GitHub).unwrap().len(), 0);
}

#[test]
fn a_failed_or_missing_chunk_read_never_removes_the_active_generation() {
    for missing in [false, true] {
        let (_scratch, path, service, store) = setup("github-read-refusal");
        let mut vault = open(&store, &service, &path);
        vault.write(Kind::GitHub, &bytes(VALID), None).unwrap();
        let active = vault.authority.current(Kind::GitHub).unwrap().active;
        let name = store.names().into_iter().next().unwrap();
        if missing {
            store.0.lock().unwrap().values.remove(&name);
        } else {
            store.0.lock().unwrap().denied_read = Some(name);
        }
        assert!(matches!(
            vault.github(Job::Load {
                stamp: Stamp { generation: 1 }
            }),
            Reply::Loaded {
                result: Err(LoadError::Unavailable),
                ..
            }
        ));
        assert_eq!(
            vault.authority.current(Kind::GitHub).unwrap().active,
            active
        );
        assert_eq!(vault.authority.retired_kind(Kind::GitHub).unwrap().len(), 0);
    }
}

#[test]
fn only_whole_bad_records_are_removed_and_the_outcome_is_preserved() {
    for value in [b"{".to_vec(), vec![b'x'; 2049]] {
        for expected in [Removed::Clean, Removed::Refused, Removed::NotDurable] {
            let (_scratch, path, service, store) = setup("github-corrupt");
            let mut vault = open(&store, &service, &path);
            vault.write(Kind::GitHub, &bytes(&value), None).unwrap();
            if expected == Removed::Refused {
                store.0.lock().unwrap().refusing = true;
            }
            if expected == Removed::NotDurable {
                refuse_commits(&path);
            }
            assert!(
                matches!(vault.github(Job::Load { stamp: Stamp { generation: 1 } }), Reply::Loaded { result: Err(LoadError::Cleanup(actual)), .. } if actual == expected)
            );
            assert_eq!(
                vault
                    .authority
                    .current(Kind::GitHub)
                    .unwrap()
                    .active
                    .is_some(),
                expected == Removed::NotDurable
            );
        }
    }
}

#[test]
fn startup_cleanup_does_not_remove_a_valid_active_record() {
    let (_scratch, path, service, store) = setup("github-active");
    let mut vault = open(&store, &service, &path);
    vault.write(Kind::GitHub, &bytes(VALID), None).unwrap();
    let old = store.0.lock().unwrap().values.clone();
    vault.write(Kind::GitHub, &bytes(VALID), None).unwrap();
    store.0.lock().unwrap().values.extend(old);
    let active = vault.authority.current(Kind::GitHub).unwrap().active;
    let Reply::Loaded {
        stamp,
        result: Ok(Some(account)),
    } = vault.github(Job::Load {
        stamp: Stamp { generation: 1 },
    })
    else {
        panic!("valid account");
    };
    assert_eq!(stamp, Stamp { generation: 1 });
    assert_eq!(account.user.id, 7);
    assert_eq!(account.user.login.as_str(), "octocat");
    assert_eq!(account.token.expose().as_str(), "ghu_fake_test_only_token");
    assert_eq!(
        vault.authority.current(Kind::GitHub).unwrap().active,
        active
    );
    assert_eq!(store.names().len(), 1);
}

#[test]
fn a_stale_save_cannot_write_and_a_current_save_precedes_removal() {
    use crate::connections::github::record;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let (_scratch, path, service, store) = setup("github-save-fence");
    let mut vault = open(&store, &service, &path);
    let fence = Arc::new(AtomicU64::new(2));
    for (generation, expected) in [(1, false), (2, true)] {
        let reply = vault.github(Job::Save {
            stamp: Stamp { generation },
            fence: Arc::clone(&fence),
            account: record::decode(&bytes(VALID)).unwrap(),
        });
        let Reply::Saved { stamp, saved } = reply else {
            panic!("save reply");
        };
        assert_eq!(stamp.generation, generation);
        assert_eq!(saved, expected);
        assert_eq!(
            vault
                .authority
                .current(Kind::GitHub)
                .unwrap()
                .active
                .is_some(),
            expected
        );
    }
    fence.store(3, Ordering::Release);
    assert!(matches!(
        vault.github(Job::Remove { generation: 3 }),
        Reply::Removed {
            generation: 3,
            result: Removed::Clean
        }
    ));
    assert_eq!(vault.authority.current(Kind::GitHub).unwrap().active, None);
    assert_eq!(store.names(), Vec::<String>::new());
}
