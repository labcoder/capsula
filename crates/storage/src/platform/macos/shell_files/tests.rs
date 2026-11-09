use super::*;

#[test]
fn names_and_prefixes_cannot_escape_or_prune_another_namespace() {
    for name in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
        assert_eq!(basename(name), Err(Untrusted::Unsafe), "{name:?}");
    }
    assert_eq!(basename("Kumochi Dev"), Ok(c"Kumochi Dev".to_owned()));
    for name in [
        b"kumochi-brain.1970-01-01.log".as_slice(),
        b"kumochi-shell-other.1970-01-01.log",
        b"kumochi-shell.1970-01-01.log.old",
        b"kumochi-shell.1970-01-01.log/child",
        b"kumochi-shell.1970-01-01.lock",
        b"kumochi-shell.1970-1-01.log",
        b"kumochi-shell.1970-01-01\0.log",
    ] {
        assert_eq!(log_date("kumochi-shell", name), None, "{name:?}");
    }
    assert_eq!(log_date("../shell", b"../shell.1970-01-01.log"), None);
}

#[test]
fn date_fallback_uses_literal_utc_boundaries() {
    assert_eq!(log_date("shell", b"shell.1970-01-01.log"), Some(UNIX_EPOCH));
    assert_eq!(
        log_date("shell", b"shell.1969-12-31.log"),
        UNIX_EPOCH.checked_sub(Duration::from_secs(86400))
    );
    assert_eq!(
        log_date("shell", b"shell.2000-02-29.log"),
        UNIX_EPOCH.checked_add(Duration::from_secs(951_782_400))
    );
    assert_eq!(
        log_date("shell", b"shell.2024-02-29.log"),
        UNIX_EPOCH.checked_add(Duration::from_secs(1_709_164_800))
    );
    for date in [
        "0000-01-01",
        "1900-02-29",
        "2100-02-29",
        "2026-00-01",
        "2026-13-01",
        "2026-04-31",
        "2026-01-00",
    ] {
        assert_eq!(
            log_date("shell", format!("shell.{date}.log").as_bytes()),
            None,
            "{date}"
        );
    }
}

#[test]
fn every_consumed_entry_spends_the_finite_budget() {
    let mut result = Retention::default();
    let mut consumed = 0;
    while result.can_read() {
        consumed += 1;
        result.scanned += 1;
        result.skipped += 1;
    }
    assert_eq!(consumed, 256);
    assert_eq!(
        result,
        Retention {
            scanned: 256,
            skipped: 256,
            removed: 0,
            incomplete: true
        }
    );
    assert!(!result.can_read());
    let mut just_below = Retention {
        scanned: 255,
        ..Retention::default()
    };
    assert!(just_below.can_read());
    assert_eq!(just_below.incomplete, false);
}

#[test]
fn creation_errors_are_not_security_inspection_failures() {
    let ordinary = AcquisitionError::Open(io::ErrorKind::StorageFull);
    let inspection = AcquisitionError::Admission(Untrusted::Io);
    assert_ne!(ordinary, inspection);
    assert_eq!(
        ordinary.to_string(),
        "private object creation or open failed"
    );
    assert_eq!(
        inspection.to_string(),
        "private object admission failed: a path component can't be opened or read"
    );
}

#[test]
fn log_writes_borrow_the_opaque_owner_through_arc() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<AdmittedLog>();
    send_sync::<AdmittedLock>();
    fn write(log: Arc<AdmittedLog>) -> io::Result<()> {
        let mut borrowed = log.as_ref();
        borrowed.write_all(b"content-free\n")?;
        borrowed.flush()
    }
    let _: fn(Arc<AdmittedLog>) -> io::Result<()> = write;
    let _: fn(&ShellStorage, &str) -> Result<AdmittedLock, AcquisitionError> =
        ShellStorage::open_instance_lock;
    let _: fn(&ShellStorage, &str, usize, &AdmittedLog) -> Result<Retention, AcquisitionError> =
        ShellStorage::retain_logs;
}

#[test]
fn volume_policy_requires_local_apfs_with_ownership_and_writable_descendants() {
    // SAFETY: zero initializes all integer fields/arrays in this fixture statfs.
    let mut metadata: libc::statfs = unsafe { std::mem::zeroed() };
    metadata.f_fstypename[..5].copy_from_slice(&[97, 112, 102, 115, 0]);
    metadata.f_flags = 0x1000;
    assert_eq!(filesystem_policy(&metadata, true), Ok(()));
    metadata.f_flags = 0x1001;
    assert_eq!(filesystem_policy(&metadata, false), Ok(()));
    assert_eq!(filesystem_policy(&metadata, true), Err(Untrusted::Unsafe));
    metadata.f_flags = 0;
    assert_eq!(filesystem_policy(&metadata, false), Err(Untrusted::Unsafe));
    metadata.f_flags = 0x20_1000;
    assert_eq!(filesystem_policy(&metadata, true), Err(Untrusted::Unsafe));
    metadata.f_flags = 0x1000;
    metadata.f_fstypename[0] = 104;
    assert_eq!(filesystem_policy(&metadata, false), Err(Untrusted::Unsafe));
}

#[test]
fn private_directory_requires_the_current_owner_and_exact_mode() {
    // SAFETY: all-zero integer stat storage is a fixture, not an inspection result.
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    metadata.st_mode = libc::S_IFDIR | 0o700;
    metadata.st_uid = 501;
    assert_eq!(private_directory(&metadata, 501), Ok(()));
    assert_eq!(private_directory(&metadata, 502), Err(Untrusted::Unsafe));
    for mode in [0o750, 0o770, 0o707, 0o1700, 0o4700] {
        metadata.st_mode = libc::S_IFDIR | mode;
        assert_eq!(private_directory(&metadata, 501), Err(Untrusted::Unsafe));
    }
}

#[test]
fn retention_routing_never_downgrades_observed_parent_refusal() {
    use std::cell::Cell;

    // SAFETY: zeroed integer stat storage is a fixture, not an acquired object.
    let mut parent: libc::stat = unsafe { std::mem::zeroed() };
    parent.st_mode = libc::S_IFDIR | 0o700;
    parent.st_uid = 501;
    let mut candidate = parent;
    candidate.st_mode = libc::S_IFREG | 0o644;
    candidate.st_nlink = 1;
    let parent = Cell::new(parent);
    let opened = Cell::new(false);
    let check_parent = || private_directory(&parent.get(), 501).map_err(AcquisitionError::from);
    let mut unsafe_parent = parent.get();
    unsafe_parent.st_mode = libc::S_IFDIR | 0o750;
    parent.set(unsafe_parent);
    let refused = checked_candidate(check_parent, || {
        opened.set(true);
        protected::safe_regular(&candidate, 501).map_err(AcquisitionError::from)
    });
    assert_eq!(refused, Err(AcquisitionError::Admission(Untrusted::Unsafe)));
    assert!(!opened.get());
    let mut restored_parent = parent.get();
    restored_parent.st_mode = libc::S_IFDIR | 0o700;
    parent.set(restored_parent);
    assert_eq!(check_parent(), Ok(()));
    assert_eq!(refused, Err(AcquisitionError::Admission(Untrusted::Unsafe)));
    assert_eq!(
        checked_candidate(check_parent, || protected::safe_regular(&candidate, 501)
            .map_err(AcquisitionError::from)),
        Ok(None)
    );
    for candidate_succeeds in [false, true] {
        parent.set(restored_parent);
        let refused_after_open = checked_candidate(check_parent, || {
            parent.set(unsafe_parent);
            if candidate_succeeds {
                Ok(())
            } else {
                protected::safe_regular(&candidate, 501).map_err(AcquisitionError::from)
            }
        });
        assert_eq!(
            refused_after_open,
            Err(AcquisitionError::Admission(Untrusted::Unsafe))
        );
        parent.set(restored_parent);
        assert_eq!(check_parent(), Ok(()));
        assert_eq!(
            refused_after_open,
            Err(AcquisitionError::Admission(Untrusted::Unsafe))
        );
    }
}
