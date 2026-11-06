use super::*;

#[test]
fn a_new_sign_in_commit_clears_only_the_old_notice_and_empty_state_is_deleted() {
    let (_scratch, path, service, store) = setup("marker-new-signin");
    let mut vault = open(&store, &service, &path);
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("promotion");
    vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: false,
        })
        .expect("checkpoint");
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("notice-only state");
    assert!(
        matches!(
            vault.update_marker(MarkerUpdate::SignInCommitted),
            Err(VaultError::Fenced)
        ),
        "no account commit yet"
    );
    vault
        .update_marker(MarkerUpdate::BeginSweep)
        .expect("new sweep");
    vault
        .update_marker(MarkerUpdate::RequestStarted)
        .expect("new request");
    vault
        .update_marker(MarkerUpdate::UnknownOutcome)
        .expect("independent sticky fact");
    committed(vault.save_account(&account("new")));
    let marker = vault
        .update_marker(MarkerUpdate::SignInCommitted)
        .expect("new token set committed");
    assert!(
        !marker.notice && marker.sweep && marker.request_pending && marker.outcome_unknown,
        "only the notice clears"
    );

    let (_scratch, path, service, store) = setup("marker-clear-notice");
    let mut vault = open(&store, &service, &path);
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("promotion");
    vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: false,
        })
        .expect("checkpoint");
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("notice-only state");
    committed(vault.save_account(&account("new")));
    vault
        .update_marker(MarkerUpdate::SignInCommitted)
        .expect("new sign-in clears old notice");
    assert_eq!(
        vault.marker().expect("marker"),
        SignOutMarker::default(),
        "nothing is left to mark"
    );
    assert_only_current(&vault, &store);
}

#[test]
fn marker_layout_is_compact_versioned_and_unknown_values_fail_closed() {
    let (_scratch, path, service, store) = setup("marker-layout");
    let mut vault = open(&store, &service, &path);
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Keep))
        .expect("promote");
    let expected = r#"{"v":1,"sign_out":{"disposition":"Keep","phase":"Revoking"},"sweep":true,"request_pending":false,"outcome_unknown":false,"notice":true}"#;
    let conn = raw(&path);
    let stored: String = conn
        .query_row("SELECT marker FROM control WHERE id = 1", [], |row| {
            row.get(0)
        })
        .expect("marker row");
    assert_eq!(stored, expected, "approved compact JSON layout");
    assert!(store.names().is_empty(), "the marker is never a credential");
    for invalid in [
        expected.replace("\"v\":1", "\"v\":2"),
        expected.replace(
            "\"sign_out\":{\"disposition\":\"Keep\",\"phase\":\"Revoking\"},",
            "",
        ),
        expected.replace("\"v\":1", "\"v\":1,\"extra\":true"),
        expected.replace("\"phase\":\"Revoking\"", "\"phase\":\"Unknown\""),
        expected.replace("\"disposition\":\"Keep\"", "\"disposition\":\"Unknown\""),
        expected.replace(
            "\"phase\":\"Revoking\"",
            "\"phase\":\"Revoking\",\"extra\":true",
        ),
        expected.replace(
            "\"notice\":true",
            &format!("\"notice\":true{}", " ".repeat(200)),
        ),
        "{}".to_owned(),
        "not json".to_owned(),
    ] {
        conn.execute("UPDATE control SET marker = ?1 WHERE id = 1", [&invalid])
            .expect("malformed fixture");
        let marker = vault.marker().expect("unreadable marker fails closed");
        assert!(
            marker
                .sign_out
                .is_some_and(|sign_out| sign_out.disposition == Disposition::Erase
                    && sign_out.phase == Phase::Revoking),
            "unknown layout fences and erases"
        );
        assert!(
            marker.sweep && marker.outcome_unknown && marker.notice,
            "unreadable state retains uncertainty"
        );
    }
}

#[test]
fn sweep_completion_preserves_every_unrelated_fact_and_a_promotion() {
    let (_scratch, path, service, store) = setup("marker-merge");
    let mut vault = open(&store, &service, &path);
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("initial sign-out");
    vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: false,
        })
        .expect("unconfirmed checkpoint");
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("notice-only state");
    vault
        .update_marker(MarkerUpdate::BeginSweep)
        .expect("sweep");
    vault
        .update_marker(MarkerUpdate::RequestStarted)
        .expect("request");
    vault
        .update_marker(MarkerUpdate::UnknownOutcome)
        .expect("uncertainty");
    let cleared = vault
        .update_marker(MarkerUpdate::FinishSweep)
        .expect("all revoke names absent");
    assert!(!cleared.sweep, "only the completed sweep clears");
    assert!(cleared.request_pending, "the outstanding request remains");
    assert!(cleared.outcome_unknown, "the unknown outcome remains");
    assert!(cleared.notice, "the prior notice remains");
    vault
        .update_marker(MarkerUpdate::BeginSweep)
        .expect("another sweep");
    let held_sweep = MarkerUpdate::FinishSweep;
    let promoted = vault
        .update_marker(MarkerUpdate::Promote(Disposition::Keep))
        .expect("promote while sweep held");
    assert!(
        promoted.request_pending && promoted.outcome_unknown && promoted.notice,
        "promotion merges existing facts"
    );
    let released = vault
        .update_marker(held_sweep)
        .expect("late sweep completion merges current state");
    assert!(
        released == promoted,
        "late sweep cannot clear sign_out or sweep"
    );
    let finished_request = vault
        .update_marker(MarkerUpdate::RequestFinished {
            outcome_unknown: false,
        })
        .expect("request result becomes owned");
    assert!(
        !finished_request.request_pending
            && finished_request.outcome_unknown
            && finished_request.notice,
        "request completion cannot reset sticky facts"
    );
}

/// The sweep flag commits with the revocation record: a refused or interrupted write sets neither.
#[test]
fn a_refused_or_interrupted_revocation_save_sets_no_sweep_marker() {
    for fault in [Some(Fault::Refuse), Some(Fault::CrashAfter), None] {
        let (_scratch, path, service, store) = setup("marker-before-data");
        let mut vault = open(&store, &service, &path);
        match fault {
            Some(fault) => store.arm(0, fault),
            None => refuse_commits(&path),
        }
        let outcome =
            panic::catch_unwind(AssertUnwindSafe(|| vault.save_revocations(&list("one"))));
        match fault {
            Some(Fault::Refuse) => assert!(
                matches!(
                    outcome.expect("refusal returns"),
                    SaveResult::NotCommitted(VaultError::Store(CredentialStoreError::Write(5)))
                ),
                "a refused write is a failed save"
            ),
            Some(Fault::CrashAfter) => assert!(outcome.is_err(), "crash after the first chunk"),
            None => {
                assert!(
                    matches!(
                        outcome.expect("refusal returns"),
                        SaveResult::NotCommitted(VaultError::Authority)
                    ),
                    "a refused commit is a failed save"
                );
                allow_commits(&path);
            }
        }
        assert!(
            !is_current(&vault, Kind::Revocations),
            "no record committed"
        );
        assert_eq!(vault.marker().expect("marker"), SignOutMarker::default());
        drop(vault);
        let mut restarted = open(&store, &service, &path);
        assert!(store.names().is_empty(), "the reopen swept every orphan");
        committed(restarted.save_revocations(&list("one")));
        assert!(
            restarted.marker().expect("marker").sweep,
            "the flag came with it"
        );
        committed(restarted.save_revocations(&Secret::new(RevocationList::default())));
        gone(restarted.delete_revocations());
        restarted
            .update_marker(MarkerUpdate::FinishSweep)
            .expect("the record is removed");
        assert!(store.names().is_empty(), "retry leaves no token data");
        assert_eq!(
            restarted.marker().expect("marker"),
            SignOutMarker::default()
        );
    }
}

#[test]
fn a_checkpoint_cannot_claim_confirmation_while_the_persisted_list_is_nonempty() {
    let (_scratch, path, service, store) = setup("checkpoint-evidence");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    committed(vault.save_revocations(&list("one")));
    let promoted = vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("promotion");
    assert!(
        matches!(
            vault.update_marker(MarkerUpdate::Checkpoint {
                list_empty_on_disk: true
            }),
            Err(VaultError::Fenced)
        ),
        "a caller cannot contradict the on-disk evidence"
    );
    assert!(
        vault.marker().expect("persisted marker") == promoted,
        "false evidence changes nothing"
    );
    assert!(
        matches!(
            vault.delete_revocations(),
            DeleteResult::Refused(VaultError::Fenced)
        ),
        "deletion stays fenced"
    );
}

#[test]
fn sweep_only_allows_account_load_but_promotion_fences_without_deleting_it() {
    let (_scratch, path, service, store) = setup("marker-fence");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    vault
        .update_marker(MarkerUpdate::BeginSweep)
        .expect("sweep");
    assert!(
        vault.load_account().expect("sweep-only load").is_some(),
        "sweep alone never fences"
    );
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("promote");
    drop(vault);
    let vault = open(&store, &service, &path);
    assert!(
        vault.load_account().expect("fenced load").is_none(),
        "a pending sign-out exposes no usable tokens"
    );
    assert!(
        is_current(&vault, Kind::Account),
        "the stored record still exists behind the fence"
    );
    assert_only_current(&vault, &store);
}

#[test]
fn checkpoint_folds_uncertainty_once_and_a_refusal_prevents_deletion() {
    for (pending, unknown, list_empty, expected_notice) in [
        (false, false, false, true),
        (false, false, true, false),
        (false, true, false, true),
        (false, true, true, true),
        (true, false, false, true),
        (true, false, true, true),
        (true, true, false, true),
        (true, true, true, true),
    ] {
        let (_scratch, path, service, store) = setup("marker-checkpoint");
        let mut vault = open(&store, &service, &path);
        committed(vault.save_account(&account("old")));
        if list_empty {
            committed(vault.save_revocations(&Secret::new(RevocationList::default())));
        }
        if pending {
            vault
                .update_marker(MarkerUpdate::RequestStarted)
                .expect("request");
        }
        if unknown {
            vault
                .update_marker(MarkerUpdate::UnknownOutcome)
                .expect("unknown");
        }
        let promoted = vault
            .update_marker(MarkerUpdate::Promote(Disposition::Erase))
            .expect("promote");
        let names = store.names();
        refuse_commits(&path);
        assert!(
            matches!(
                vault.update_marker(MarkerUpdate::Checkpoint {
                    list_empty_on_disk: list_empty
                }),
                Err(VaultError::Authority)
            ),
            "refused checkpoint is reported"
        );
        assert!(
            vault.marker().expect("marker after refusal") == promoted,
            "refused write changes no control fact"
        );
        assert!(
            matches!(
                vault.delete_account(),
                DeleteResult::Refused(VaultError::Fenced)
            ),
            "no delete starts before a durable checkpoint"
        );
        allow_commits(&path);
        assert_eq!(
            store.names(),
            names,
            "all token names survive a refused checkpoint"
        );
        let checkpoint = vault
            .update_marker(MarkerUpdate::Checkpoint {
                list_empty_on_disk: list_empty,
            })
            .expect("checkpoint retry");
        assert!(
            checkpoint
                .sign_out
                .is_some_and(|sign_out| sign_out.phase == Phase::Deleting),
            "checkpoint commits the deletion phase"
        );
        assert_eq!(checkpoint.notice, expected_notice);
        assert!(
            !checkpoint.request_pending && !checkpoint.outcome_unknown && checkpoint.sweep,
            "uncertainty folds into the decided notice"
        );
        let repeated = vault
            .update_marker(MarkerUpdate::Checkpoint {
                list_empty_on_disk: true,
            })
            .expect("idempotent checkpoint");
        assert!(
            repeated == checkpoint,
            "later empty state cannot change the decision"
        );
    }
}

#[test]
fn a_refused_promotion_preserves_the_sweep_marker_and_every_token_name() {
    let (_scratch, path, service, store) = setup("marker-promotion-refusal");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_account(&account("old")));
    committed(vault.save_revocations(&list("one")));
    let names = store.names();
    refuse_commits(&path);
    assert!(
        matches!(
            vault.update_marker(MarkerUpdate::Promote(Disposition::Erase)),
            Err(VaultError::Authority)
        ),
        "refused promotion retains its exact error"
    );
    allow_commits(&path);
    let marker = vault.marker().expect("persisted marker");
    assert!(
        marker.sign_out.is_none()
            && marker.sweep
            && !marker.request_pending
            && !marker.outcome_unknown
            && !marker.notice,
        "only the existing sweep fact remains"
    );
    assert_eq!(
        store.names(),
        names,
        "promotion refusal changes no token names"
    );
    assert!(
        vault
            .load_account()
            .expect("ordinary account read")
            .is_some(),
        "a refused promotion is not a durable sign-out"
    );
}

#[test]
fn a_refused_finish_preserves_the_checkpointed_outcome_and_a_retry_finishes() {
    for (confirmed, expected_notice) in [(false, true), (true, false)] {
        let (_scratch, path, service, store) = setup("marker-final-fault");
        let mut vault = open(&store, &service, &path);
        committed(vault.save_account(&account("old")));
        committed(vault.save_revocations(&list("one")));
        vault
            .update_marker(MarkerUpdate::Promote(Disposition::Erase))
            .expect("promotion");
        if confirmed {
            committed(vault.save_revocations(&Secret::new(RevocationList::default())));
        }
        let decided = vault
            .update_marker(MarkerUpdate::Checkpoint {
                list_empty_on_disk: confirmed,
            })
            .expect("checkpoint");
        gone(vault.delete_account());
        gone(vault.delete_revocations());
        assert!(
            vault.marker().expect("marker after deletion") == decided,
            "deletion never changes the decided outcome"
        );
        assert!(!is_current(&vault, Kind::Account) && !is_current(&vault, Kind::Revocations));
        drop(vault);
        let mut vault = open(&store, &service, &path);
        assert!(
            vault.marker().expect("post-delete restart") == decided,
            "R4-H1 warning survives a crash before Finish"
        );
        refuse_commits(&path);
        assert!(
            matches!(
                vault.update_marker(MarkerUpdate::FinishSignOut),
                Err(VaultError::Authority)
            ),
            "a refused finish is reported"
        );
        allow_commits(&path);
        drop(vault);
        let mut restarted = open(&store, &service, &path);
        assert!(
            restarted.marker().expect("refused finish marker") == decided,
            "a refused finish remains Deleting"
        );
        restarted
            .update_marker(MarkerUpdate::FinishSignOut)
            .expect("retry final write");
        let marker = restarted.marker().expect("finished marker");
        assert!(
            marker.sign_out.is_none()
                && !marker.sweep
                && !marker.request_pending
                && !marker.outcome_unknown,
            "only the decided notice can remain"
        );
        assert_eq!(marker.notice, expected_notice);
        assert!(store.names().is_empty(), "no token-bearing names survive");
    }
}

/// Absence is the authority's: a name it doesn't hold current never blocks the finish, and
/// the finish reads no credential.
#[test]
fn finish_needs_every_record_removed_and_reads_no_name() {
    let (_scratch, path, service, store) = setup("marker-absence-check");
    let mut vault = open(&store, &service, &path);
    committed(vault.save_revocations(&list("one")));
    let names = store.names();
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Erase))
        .expect("promote");
    let decided = vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: false,
        })
        .expect("checkpoint");
    assert!(
        matches!(
            vault.update_marker(MarkerUpdate::FinishSignOut),
            Err(VaultError::NotGone)
        ),
        "a current record keeps the deletion pending"
    );
    assert_eq!(vault.marker().expect("marker"), decided);
    store.0.lock().expect("fake lock").refusing = true;
    gone(vault.delete_revocations());
    store.0.lock().expect("fake lock").refusing = false;
    assert_eq!(store.names(), names, "the refused deletes left every name");
    store.0.lock().expect("fake lock").denied_read = Some(names[0].clone());
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("the removal committed, whatever the store still holds");
    assert!(
        vault.marker().expect("marker").notice,
        "only the warning remains"
    );
}

#[test]
fn keep_rewrite_requires_the_checkpoint_and_removes_email_plan_and_every_token_name() {
    let (_scratch, path, service, store) = setup("marker-keep");
    let mut vault = open(&store, &service, &path);
    let original = account("old");
    committed(vault.save_account(&original));
    vault
        .update_marker(MarkerUpdate::Promote(Disposition::Keep))
        .expect("promote");
    assert!(
        matches!(
            vault.save_account(&registration(&original)),
            SaveResult::NotCommitted(VaultError::Fenced)
        ),
        "chatgpt stays unchanged during Revoking"
    );
    committed(vault.save_revocations(&Secret::new(RevocationList::default())));
    vault
        .update_marker(MarkerUpdate::Checkpoint {
            list_empty_on_disk: true,
        })
        .expect("checkpoint");
    let fenced = vault
        .load_for_sign_out()
        .expect("read during Deleting")
        .expect("account still present");
    committed(vault.save_account(&registration(&fenced)));
    assert_only_current(&vault, &store);
    gone(vault.delete_revocations());
    vault
        .update_marker(MarkerUpdate::FinishSignOut)
        .expect("registration-only slot is the sole allowed exception");
    let actual = vault
        .load_account()
        .expect("registration")
        .expect("retained");
    assert!(
        actual.expose() == registration(&original).expose(),
        "only client and subject survive Keep"
    );
    assert_eq!(
        vault.marker().expect("marker"),
        SignOutMarker::default(),
        "a confirmed finish leaves no marker"
    );
    assert_only_current(&vault, &store);
}
