use super::*;
use crate::connections::github::{
    GitHubToken, RefreshToken,
    device::{Account, Removed, Stamp},
    lane::{Job, LoadError, Reply},
    record,
};

fn bytes(value: &[u8]) -> SecretBytes {
    Secret::new(Zeroizing::new(value.to_vec()))
}

const VALID: &[u8] = br#"{"token":"ghu_fake_test_only_token","refresh":"ghr_fake_test_only_token","expires_at_s":1700028800,"refresh_expires_at_s":1715897600,"refresh_sent":false,"login":"octocat","id":7}"#;

#[test]
fn a_refused_github_removal_stays_visible_after_restart_until_cleanup_succeeds() {
    let (_scratch, path, service, store) = setup("github-restart-refused");
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
    drop(vault);
    let mut vault = open(&store, &service, &path);
    assert_eq!(vault.authority.current(Kind::GitHub).unwrap().active, None);
    assert!(matches!(
        vault.github(Job::Load {
            stamp: Stamp { generation: 2 }
        }),
        Reply::Loaded {
            stamp: Stamp { generation: 2 },
            result: Err(LoadError::Cleanup(Removed::Refused))
        }
    ));
    assert_eq!(store.names().len(), 1);
    store.0.lock().unwrap().refusing = false;
    assert!(matches!(
        vault.github(Job::Load {
            stamp: Stamp { generation: 3 }
        }),
        Reply::Loaded {
            stamp: Stamp { generation: 3 },
            result: Ok(None)
        }
    ));
    assert_eq!(store.names(), Vec::<String>::new());
}

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
    for value in [
        b"{".to_vec(),
        vec![b'x'; 2049],
        br#"{"token":"ghu_fake_test_only_token","login":"octocat","id":7}"#.to_vec(),
    ] {
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
    assert_eq!(
        account.credentials.token.expose().as_str(),
        "ghu_fake_test_only_token"
    );
    assert_eq!(
        vault.authority.current(Kind::GitHub).unwrap().active,
        active
    );
    assert_eq!(store.names().len(), 1);
}

#[test]
fn a_stale_save_cannot_write_and_a_current_save_precedes_removal() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let (_scratch, path, service, store) = setup("github-save-fence");
    let mut vault = open(&store, &service, &path);
    assert!(matches!(
        vault.github(Job::Load {
            stamp: Stamp { generation: 1 }
        }),
        Reply::Loaded {
            stamp: Stamp { generation: 1 },
            result: Ok(None)
        }
    ));
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

fn loaded_account(vault: &mut Vault<FakeStore>) -> Account {
    let Reply::Loaded {
        stamp: Stamp { generation: 9 },
        result: Ok(Some(account)),
    } = vault.github(Job::Load {
        stamp: Stamp { generation: 9 },
    })
    else {
        panic!("complete persisted account");
    };
    account
}

#[test]
fn restart_preserves_a_spent_refresh_and_replaces_the_pair_only_after_a_successful_save() {
    use std::sync::{Arc, atomic::AtomicU64};

    let (_scratch, path, service, store) = setup("github-refresh-restart");
    let mut vault = open(&store, &service, &path);
    vault.write(Kind::GitHub, &bytes(VALID), None).unwrap();
    let mut spent = record::decode(&bytes(VALID)).unwrap();
    spent.refresh_sent = true;
    assert!(matches!(
        vault.github(Job::Save {
            stamp: Stamp { generation: 1 },
            fence: Arc::new(AtomicU64::new(1)),
            account: spent,
        }),
        Reply::Saved {
            stamp: Stamp { generation: 1 },
            saved: true
        }
    ));
    drop(vault);
    let mut vault = open(&store, &service, &path);
    let spent = loaded_account(&mut vault);
    assert!(spent.refresh_sent);
    assert_eq!(
        spent.credentials.token.expose().as_str(),
        "ghu_fake_test_only_token"
    );
    assert_eq!(
        spent.credentials.refresh.expose().as_str(),
        "ghr_fake_test_only_token"
    );
    assert_eq!(spent.credentials.expires_at_s, 1_700_028_800);
    assert_eq!(spent.credentials.refresh_expires_at_s, 1_715_897_600);
    let mut rotated = spent;
    rotated.credentials.token = Secret::new(
        GitHubToken::new(Zeroizing::new("ghu_rotated_test_only_token".to_owned())).unwrap(),
    );
    rotated.credentials.refresh = Secret::new(
        RefreshToken::new(Zeroizing::new("ghr_rotated_test_only_token".to_owned())).unwrap(),
    );
    rotated.credentials.expires_at_s += 3600;
    rotated.credentials.refresh_expires_at_s += 3600;
    rotated.refresh_sent = false;
    store.arm(0, Fault::Refuse);
    assert!(matches!(
        vault.github(Job::Save {
            stamp: Stamp { generation: 2 },
            fence: Arc::new(AtomicU64::new(2)),
            account: rotated.clone(),
        }),
        Reply::Saved {
            stamp: Stamp { generation: 2 },
            saved: false
        }
    ));
    drop(vault);
    let mut vault = open(&store, &service, &path);
    let unchanged = loaded_account(&mut vault);
    assert!(unchanged.refresh_sent);
    assert_eq!(
        unchanged.credentials.token.expose().as_str(),
        "ghu_fake_test_only_token"
    );
    assert_eq!(
        unchanged.credentials.refresh.expose().as_str(),
        "ghr_fake_test_only_token"
    );
    assert_eq!(unchanged.credentials.expires_at_s, 1_700_028_800);
    assert_eq!(unchanged.credentials.refresh_expires_at_s, 1_715_897_600);
    assert!(matches!(
        vault.github(Job::Save {
            stamp: Stamp { generation: 2 },
            fence: Arc::new(AtomicU64::new(2)),
            account: rotated,
        }),
        Reply::Saved {
            stamp: Stamp { generation: 2 },
            saved: true
        }
    ));
    drop(vault);
    let mut vault = open(&store, &service, &path);
    let restored = loaded_account(&mut vault);
    assert!(!restored.refresh_sent);
    assert_eq!(
        restored.credentials.token.expose().as_str(),
        "ghu_rotated_test_only_token"
    );
    assert_eq!(
        restored.credentials.refresh.expose().as_str(),
        "ghr_rotated_test_only_token"
    );
    assert_eq!(restored.credentials.expires_at_s, 1_700_032_400);
    assert_eq!(restored.credentials.refresh_expires_at_s, 1_715_901_200);
    assert_eq!(restored.user.id, 7);
    assert_eq!(restored.user.login.as_str(), "octocat");
}
