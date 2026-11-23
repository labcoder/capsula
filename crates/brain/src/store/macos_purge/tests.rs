use std::fs;
use std::io::{Seek as _, SeekFrom};

use crate::store::{Cancel, Copied};
use crate::test_support::Scratch;
use rusqlite::config::DbConfig;

use super::*;

fn staged(label: &str) -> (Scratch, Store, Copied) {
    let scratch = Scratch::new(label);
    let mut store = Store::open(&scratch.db()).expect("admitted store");
    store
        .conn
        .execute("INSERT INTO purge_pending VALUES (1)", [])
        .expect("record purge debt");
    store.purge_pending = true;
    let copied = store
        .owed()
        .expect("read purge debt")
        .expect("purge is due")
        .copy(&Arc::new(Cancel::default()))
        .expect("build actual compacted successor")
        .expect("copy completes");
    (scratch, store, copied)
}

fn restart(path: &Path, intent: Intent, role: Role) {
    let store = Store::open(path).expect("reconcile before reopening");
    let expected = match role {
        Role::Old => (intent.old, true),
        Role::Successor => (intent.new, false),
    };
    assert_eq!(
        (
            stamp(&store.conn).expect("canonical stamp"),
            store.purge_pending()
        ),
        expected,
        "restart must select the verified generation and preserve its own purge debt"
    );
    assert!(!sidecar(path, INTENT).exists(), "durably retired intent");
    if role == Role::Successor && store.leftovers().exist() {
        store
            .leftovers()
            .remove()
            .expect("remove the old generation after confirming its store");
    }
    assert!(!store.leftovers().exist(), "no cleanup debt remains");
}

fn published(store: &mut Store, copied: &Copied) {
    let error = promote_with(
        store,
        copied.writes,
        copied.macos.as_ref().expect("bound copy"),
        |files| files.dir.link(files.db, &files.old),
        |boundary| {
            if boundary == Boundary::Reopen {
                return Err(interrupted());
            }
            Ok(())
        },
    )
    .expect_err("leave a published but unavailable attempt");
    assert!(
        matches!(error, StoreError::Purge(ref error) if error.raw_os_error() == Some(libc::EIO)),
        "the injected reopen fault is reported: {error:?}"
    );
}

fn interrupted() -> io::Error {
    io::Error::from_raw_os_error(libc::EIO)
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn restart_after_link_rename_and_flush_chooses_the_stamped_generation() {
    for boundary in [Boundary::Link, Boundary::Rename, Boundary::Flush] {
        let (scratch, store, copied) = staged("purge-phase");
        let path = scratch.db();
        let copied = copied.macos.expect("bound copy");
        assert!(checkpoint_now(&store.conn).expect("checkpoint old"));
        drop(store);
        let files = Files::open(&path).unwrap().unwrap();
        let old = files.dir.hold(&path).unwrap();
        let clean = files.dir.hold(&files.clean).unwrap();
        old.sync().unwrap();
        clean.sync().unwrap();
        files.publish_intent(copied.intent).unwrap();
        files.dir.link(&path, &files.old).unwrap();
        let error = files
            .publish_linked(&old, &clean, |reached| {
                if reached == boundary {
                    return Err(interrupted());
                }
                Ok(())
            })
            .expect_err("stop before the next boundary");
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(files.intent.is_file() && files.old.is_file());
        drop((old, clean, files));
        if boundary == Boundary::Link {
            restart(&path, copied.intent, Role::Old);
        } else {
            restart(&path, copied.intent, Role::Successor);
        }
    }
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn published_but_unavailable_reopens_without_retrying_publication() {
    let (scratch, mut store, copied) = staged("purge-reopen");
    let path = scratch.db();
    let copied_mac = copied.macos.as_ref().unwrap();
    let journal = sidecar(&path, "-journal");
    let error = promote_with(
        &mut store,
        copied.writes,
        copied_mac,
        |files| files.dir.link(files.db, &files.old),
        |boundary| {
            if boundary == Boundary::Reopen {
                fs::create_dir(&journal)?;
            }
            Ok(())
        },
    )
    .expect_err("reopen refuses an unexpected journal after verified publication");
    assert!(
        matches!(error, StoreError::Purge(ref error) if error.kind() == ErrorKind::InvalidData)
    );
    assert!(!store.serving, "no writes before successor verification");
    assert_eq!(
        store
            .leftovers()
            .remove()
            .expect_err("must retain evidence")
            .kind(),
        ErrorKind::WouldBlock
    );
    assert!(sidecar(&path, OLD).is_file() && sidecar(&path, INTENT).is_file());
    fs::remove_dir(journal).expect("remove the owned fault fixture");
    store
        .reconnect()
        .expect("open the already published successor");
    assert_eq!(stamp(&store.conn).unwrap(), copied_mac.intent.new);
    assert!(!store.purge_pending());
    drop(store);
    restart(&path, copied_mac.intent, Role::Successor);
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn restart_after_unlink_finishes_cleanup_before_retiring_intent() {
    let (scratch, mut store, copied) = staged("purge-unlink");
    let path = scratch.db();
    published(&mut store, &copied);
    let error = reconcile_with(&path, store.mac_published, |boundary| {
        assert_eq!(boundary, Boundary::Unlink);
        Err(interrupted())
    })
    .expect_err("interrupt after unlink but before the durability barrier");
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    assert!(
        sidecar(&path, OLD).is_file(),
        "the published old generation stays an ordinary leftover"
    );
    assert!(sidecar(&path, INTENT).is_file());
    drop(store);
    restart(&path, copied.macos.unwrap().intent, Role::Successor);
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn missing_or_corrupt_canonical_refuses_create_migration_and_cleanup() {
    let (scratch, mut store, copied) = staged("purge-ambiguous");
    let path = scratch.db();
    published(&mut store, &copied);
    drop(store);
    let alias = sidecar(&path, OLD);
    let intent = sidecar(&path, INTENT);
    let expected_alias = fs::read(&alias).unwrap();
    let expected_intent = fs::read(&intent).unwrap();
    fs::rename(&path, sidecar(&path, ".saved-successor")).unwrap();
    assert!(
        matches!(Store::open(&path), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::NotFound)
    );
    assert!(
        !path.exists(),
        "a missing primary must never create an empty store"
    );
    fs::write(&path, b"not a SQLite database").unwrap();
    assert!(
        matches!(Store::open(&path), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::InvalidData)
    );
    assert_eq!(fs::read(&path).unwrap(), b"not a SQLite database");
    assert_eq!(fs::read(alias).unwrap(), expected_alias);
    assert_eq!(fs::read(intent).unwrap(), expected_intent);
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn substituted_alias_and_applicable_wal_preserve_intent_and_candidates() {
    let (scratch, mut store, copied) = staged("purge-substituted");
    let path = scratch.db();
    published(&mut store, &copied);
    drop(store);
    let old = sidecar(&path, OLD);
    let intent = sidecar(&path, INTENT);
    let displaced = sidecar(&path, ".held-old");
    let other = Scratch::new("purge-unrelated");
    drop(Store::open(&other.db()).expect("stamp unrelated DB"));
    fs::rename(&old, &displaced).unwrap();
    fs::copy(other.db(), &old).unwrap();
    let unexpected = fs::read(&old).unwrap();
    let record = fs::read(&intent).unwrap();
    assert!(
        matches!(Store::open(&path), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::InvalidData)
    );
    assert_eq!(fs::read(&old).unwrap(), unexpected);
    assert_eq!(fs::read(&intent).unwrap(), record);

    fs::rename(displaced, &old).unwrap();
    let wal = sidecar(&path, "-wal");
    fs::write(&wal, b"unexpected nonempty WAL").unwrap();
    assert!(
        matches!(Store::open(&path), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::InvalidData)
    );
    assert!(
        old.is_file(),
        "the old alias still protects the paused attempt"
    );
    assert_eq!(fs::read(&wal).unwrap(), b"unexpected nonempty WAL");
    assert_eq!(fs::read(&intent).unwrap(), record);
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn unsupported_link_keeps_old_generation_and_purge_debt() {
    let (scratch, mut store, copied) = staged("purge-link-refused");
    let copied_mac = copied.macos.as_ref().unwrap();
    let error = promote_with(
        &mut store,
        copied.writes,
        copied_mac,
        |_| Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
        |step| {
            assert_eq!(
                step,
                Boundary::Closed,
                "unsupported link must not pass the link"
            );
            Ok(())
        },
    )
    .expect_err("no no-link publication on APFS");
    assert!(
        matches!(error, StoreError::Purge(error) if error.raw_os_error() == Some(libc::ENOTSUP))
    );
    assert!(store.serving && store.purge_pending());
    assert_eq!(stamp(&store.conn).unwrap(), copied_mac.intent.old);
    assert!(!store.leftovers().exist());
    assert!(!sidecar(&scratch.db(), OLD).exists());
}

#[test]
#[ignore = "requires supervision; a rename with an interrupted barrier must exit 78"]
fn indeterminate_purge_exits_with_all_named_evidence() {
    let (_scratch, mut store, copied) = staged("purge-indeterminate");
    let copied_mac = copied.macos.as_ref().unwrap();
    let result = promote_with(
        &mut store,
        copied.writes,
        copied_mac,
        |files| files.dir.link(files.db, &files.old),
        |boundary| {
            if boundary == Boundary::Rename {
                return Err(interrupted());
            }
            Ok(())
        },
    );
    panic!("rename followed by an interrupted barrier must exit 78, returned {result:?}");
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn successor_crash_does_not_strand_wal_or_new_purge_debt() {
    for checkpoint in [false, true] {
        let (scratch, mut store, copied) = staged("purge-renewed-writes");
        let path = scratch.db();
        assert_eq!(store.swap(&copied).expect("publish"), Swap::Done);
        store
            .conn
            .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
            .expect("simulate a crash leaving the renewed WAL");
        store
            .conn
            .execute_batch(
                "BEGIN; CREATE TABLE renewed (value TEXT);
             INSERT INTO renewed VALUES ('after swap');
             INSERT INTO purge_pending VALUES (1); COMMIT;",
            )
            .expect("write a receipt and fresh purge debt on the successor");
        if checkpoint {
            assert!(checkpoint_now(&store.conn).expect("checkpoint the new write"));
        }
        drop(store);
        assert_eq!(
            fs::metadata(sidecar(&path, "-wal"))
                .expect("retained WAL")
                .len()
                > 0,
            !checkpoint,
            "both uncheckpointed and checkpointed renewed writes must be covered"
        );
        let store = Store::open(&path).expect("a renewed write must not strand the successor");
        assert!(
            store.purge_pending(),
            "new purge debt is not the old intent"
        );
        let receipt: String = store
            .conn
            .query_row("SELECT value FROM renewed", [], |row| row.get(0))
            .unwrap();
        assert_eq!(receipt, "after swap");
        assert!(!sidecar(&path, INTENT).exists());
    }
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn prelink_stale_clone_cannot_pose_as_the_original_store() {
    let (scratch, store, copied) = staged("purge-stale-inode");
    let path = scratch.db();
    let copied = copied.macos.expect("bound clean candidate");
    assert!(checkpoint_now(&store.conn).expect("checkpoint for restart"));
    drop(store);
    let files = Files::open(&path).unwrap().unwrap();
    files
        .publish_intent(copied.intent)
        .expect("commit intent before linking");
    let displaced = sidecar(&path, ".actual-old");
    fs::rename(&path, &displaced).unwrap();
    fs::copy(&displaced, &path).unwrap();
    assert!(
        matches!(Store::open(&path), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::InvalidData),
        "a copied stamp is not the original canonical inode"
    );
    assert!(files.clean.is_file() && files.intent.is_file() && displaced.is_file());
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn damaged_live_table_preserves_the_old_candidate() {
    let (scratch, mut store, copied) = staged("purge-live-table");
    let path = scratch.db();
    let root_page: i64 = store
        .conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name = 'events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let page_size: i64 = store
        .conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    assert!(
        root_page > 1,
        "damage a live table page rather than the database header"
    );
    let mac = copied.macos.as_ref().unwrap();
    let stopped = promote_with(
        &mut store,
        copied.writes,
        mac,
        |files| files.dir.link(files.db, &files.old),
        |boundary| {
            if boundary == Boundary::Reopen {
                Err(interrupted())
            } else {
                Ok(())
            }
        },
    );
    assert!(stopped.is_err() && !store.serving);
    drop(store);
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(
        u64::try_from((root_page - 1) * page_size).unwrap(),
    ))
    .unwrap();
    file.write_all(&[0]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(
        matches!(Store::open(&path), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::InvalidData),
        "the stamp page and header still read, but a required live table is corrupt"
    );
    assert!(sidecar(&path, OLD).is_file() && sidecar(&path, INTENT).is_file());
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn unexpected_journal_blocks_the_old_writer_and_reconnect() {
    let (scratch, mut store, copied) = staged("purge-unexpected-journal");
    let path = scratch.db();
    let journal = sidecar(&path, "-journal");
    let bytes = b"foreign rollback journal sentinel";
    let result = promote_with(
        &mut store,
        copied.writes,
        copied.macos.as_ref().unwrap(),
        |_| panic!("nothing can link beside an unexpected journal"),
        |boundary| {
            if boundary == Boundary::Closed {
                fs::write(&journal, bytes)
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    assert!(
        !store.serving,
        "a journal refusal must not re-enable the old writer"
    );
    assert!(
        store.reconnect().is_err(),
        "reconnect cannot apply an unexplained journal"
    );
    assert_eq!(fs::read(&journal).unwrap(), bytes);
    assert!(
        store.high_water().is_err(),
        "the abandoned store connection is unavailable"
    );
    fs::remove_file(journal).unwrap();
    store
        .reconnect()
        .expect("open the same valid old inode after the journal is absent");
    assert!(store.purge_pending());
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn preintent_foreign_copy_and_orphan_journal_are_preserved() {
    let home = Scratch::new("purge-foreign-home");
    let store = Store::open(&home.db()).expect("admitted canonical");
    assert!(checkpoint_now(&store.conn).unwrap());
    let other = Scratch::new("purge-foreign-other");
    let foreign = Store::open(&other.db()).expect("unrelated store");
    assert!(checkpoint_now(&foreign.conn).unwrap());
    drop(foreign);
    let clean = sidecar(&home.db(), CLEAN);
    fs::copy(other.db(), &clean).unwrap();
    let bytes = fs::read(&clean).unwrap();
    let error = store
        .leftovers()
        .remove()
        .expect_err("foreign copy is not cleanup debt");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(fs::read(&clean).unwrap(), bytes);
    let orphan = sidecar(&sidecar(&home.db(), SNAPSHOT), "-wal");
    drop(store);
    fs::remove_file(clean).unwrap();
    fs::remove_file(home.db()).unwrap();
    fs::write(&orphan, b"orphan WAL").unwrap();
    assert!(
        matches!(Store::open(&home.db()), Err(StoreError::Purge(error)) if error.kind() == ErrorKind::InvalidData)
    );
    assert!(!home.db().exists());
    assert_eq!(fs::read(orphan).unwrap(), b"orphan WAL");
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn canonical_link_is_refused_before_migration_or_first_stamp() {
    let source = Scratch::new("purge-link-source");
    fs::create_dir_all(source.dir()).unwrap();
    let mut conn = Connection::open(source.db()).unwrap();
    schema::migrate_to(&mut conn, 2).unwrap();
    drop(conn);
    let expected = fs::read(source.db()).unwrap();
    for symlink in [true, false] {
        let linked = Scratch::new("purge-canonical-link");
        fs::create_dir_all(linked.dir()).unwrap();
        if symlink {
            std::os::unix::fs::symlink(source.db(), linked.db()).unwrap();
        } else {
            fs::hard_link(source.db(), linked.db()).unwrap();
        }
        assert!(
            matches!(Store::open(&linked.db()), Err(StoreError::Purge(_))),
            "a canonical link must be refused before the first SQLite write"
        );
        assert_eq!(fs::read(source.db()).unwrap(), expected);
    }
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn failed_intent_retirement_flush_is_retried_after_unlink() {
    let (scratch, mut store, copied) = staged("purge-retirement-flush");
    let path = scratch.db();
    let copied_mac = copied.macos.as_ref().unwrap();
    let stopped = promote_with(
        &mut store,
        copied.writes,
        copied_mac,
        |files| files.dir.link(files.db, &files.old),
        |boundary| {
            if boundary == Boundary::Reopen {
                Err(interrupted())
            } else {
                Ok(())
            }
        },
    );
    assert!(stopped.is_err() && !store.serving);
    let active = store.mac_published.expect("confirmed publication");
    let mut barriers = 0;
    for _ in 0..2 {
        let error = reconcile_with(&path, Some(active), |boundary| {
            if boundary == Boundary::Retire {
                barriers += 1;
                return Err(interrupted());
            }
            Ok(())
        })
        .expect_err("the cleanup retry must not forget the missing durability barrier");
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(!sidecar(&path, INTENT).exists());
    }
    assert_eq!(barriers, 2, "expect one real final flush attempt per retry");
}

#[test]
#[ignore = "requires an owned fixture directory for failed-open diagnostics"]
fn sqlite_open_diagnostics_keep_the_code_but_not_the_filename() {
    let scratch = Scratch::new("purge-log-invalid");
    fs::create_dir_all(scratch.dir()).unwrap();
    let sentinel = "private-fixture-filename-sentinel";
    let path = scratch.dir().join(sentinel);
    for open in [open_canonical, open_snapshot] {
        let error = open(&path).expect_err("no-create open of an absent file");
        assert!(
            !format!("{error:?} {error}").contains(sentinel),
            "the open error reaches the purge log without source path: {error:?}"
        );
        assert!(
            format!("{error:?}").contains("CannotOpen (14)"),
            "SQLite CANTOPEN code: {error:?}"
        );
    }
}
