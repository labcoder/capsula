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
        UNIX_EPOCH.checked_sub(Duration::from_hours(24))
    );
    assert_eq!(
        log_date("shell", b"shell.2000-02-29.log"),
        UNIX_EPOCH.checked_add(Duration::from_hours(264_384))
    );
    assert_eq!(
        log_date("shell", b"shell.2024-02-29.log"),
        UNIX_EPOCH.checked_add(Duration::from_hours(474_768))
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
fn dirent_names_use_only_the_name_and_terminator_inside_the_record() {
    let header = u16::try_from(DIRENT_NAME_OFFSET).unwrap();
    let bytes = b"ab\0padding";
    assert_eq!(
        bounded_dirent_name(header + 3, 2, |len| &bytes[..len]),
        Some(c"ab")
    );
    for bytes in [b"abc", b"a\0\0"] {
        assert_eq!(
            bounded_dirent_name(header + 3, 2, |len| &bytes[..len]),
            None
        );
    }
    let mut longest = [b'a'; 1024];
    longest[1023] = 0;
    assert_eq!(
        bounded_dirent_name(header + 1024, 1023, |len| &longest[..len])
            .map(|name| name.to_bytes().len()),
        Some(1023)
    );
}

#[test]
fn dirent_name_bounds_refuse_without_reading_the_name() {
    let header = u16::try_from(DIRENT_NAME_OFFSET).unwrap();
    for (reclen, namlen) in [
        (header - 1, 0),
        (header, 0),
        (header + 2, 2),
        (header + 1025, 1024),
    ] {
        assert_eq!(
            bounded_dirent_name(reclen, namlen, |_| panic!("invalid name extent")),
            None,
            "{reclen}/{namlen}"
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
    assert!(!just_below.incomplete);
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
    let _: fn(Arc<AdmittedLog>) -> io::Result<()> = |log| {
        let mut borrowed = log.as_ref();
        borrowed.write_all(b"content-free\n")?;
        borrowed.flush()
    };
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

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_a_folder_keeps_its_name_while_its_link_count_changes() {
    use crate::platform::macos::protected::tests::Scratch;

    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let busy = scratch.directory(c"busy");
    let before = protected::stat(busy.as_raw_fd()).expect("folder identity");
    // SAFETY: busy is a held fixture folder and the name is fixed; creation is exclusive.
    let made = unsafe { libc::mkdirat(busy.as_raw_fd(), c"child".as_ptr(), 0o700) };
    assert_eq!(made, 0, "a sibling makes a subfolder");
    scratch.record(&busy, c"child").expect("record subfolder");
    let after = protected::stat(busy.as_raw_fd()).expect("folder identity");
    assert_ne!(
        before.st_nlink, after.st_nlink,
        "the fixture moved the link count"
    );
    assert_eq!(named(&root, c"busy", &before), Ok(()));

    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: root is the held scratch folder and the name is fixed; creation is exclusive.
    let fd = unsafe { libc::openat(root.as_raw_fd(), c"file".as_ptr(), flags, 0o600) };
    assert!(fd >= 0, "exclusive fixture file");
    // SAFETY: the successful open transferred a fresh, uniquely owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    scratch.record(&root, c"file").expect("record file");
    let single = protected::stat(file.as_raw_fd()).expect("file identity");
    // SAFETY: root is the held scratch folder; both names are fixed fixture components.
    let linked = unsafe {
        libc::linkat(
            root.as_raw_fd(),
            c"file".as_ptr(),
            root.as_raw_fd(),
            c"second".as_ptr(),
            0,
        )
    };
    assert_eq!(linked, 0, "fixture hard link");
    scratch.record(&root, c"second").expect("record hard link");
    assert_eq!(
        named(&root, c"file", &single),
        Err(Untrusted::Unsafe),
        "a file that gained a name"
    );

    drop((busy, file));
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}
