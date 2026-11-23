use std::fs;

use crate::store::{Cancel, Copied};
use crate::test_support::Scratch;

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
    if role == Role::Successor {
        assert!(
            store.leftovers().exist(),
            "verified successor leaves cleanup debt"
        );
        store
            .leftovers()
            .remove()
            .expect("remove only verified cleanup debt");
    }
    assert!(!store.leftovers().exist(), "durably retired intent");
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
    assert_eq!(store.swap(&copied).expect("publish"), Swap::Done);
    let error = reconcile_with(&path, store.mac_published, |boundary| {
        assert_eq!(boundary, Boundary::Unlink);
        Err(interrupted())
    })
    .expect_err("interrupt after unlink but before the durability barrier");
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    assert!(!sidecar(&path, OLD).exists());
    assert!(sidecar(&path, INTENT).is_file());
    drop(store);
    restart(&path, copied.macos.unwrap().intent, Role::Successor);
}

#[test]
#[ignore = "requires an admitted APFS fixture directory"]
fn missing_or_corrupt_canonical_refuses_create_migration_and_cleanup() {
    let (scratch, mut store, copied) = staged("purge-ambiguous");
    let path = scratch.db();
    assert_eq!(store.swap(&copied).expect("publish"), Swap::Done);
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
    assert_eq!(store.swap(&copied).expect("publish"), Swap::Done);
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
        |step| panic!("unsupported link must not reach {step:?}"),
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
