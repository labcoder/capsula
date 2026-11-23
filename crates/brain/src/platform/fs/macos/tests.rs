use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::ptr::NonNull;

use super::custody::acl_absent;
use super::{
    Fault, Publication, Scratch, Stage, available_bytes, capacity, check_volume, links_unsupported,
    named_stat, open_at, rename, unlink,
};

fn approved_parent() -> File {
    let path =
        std::env::var_os("MAC_SCRATCH_TEST_PARENT").expect("explicit owned native fixture parent");
    assert!(
        std::path::Path::new(&path).is_absolute(),
        "absolute native fixture parent"
    );
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .expect("held native fixture parent")
}

fn fixture() -> Scratch {
    let random = getrandom::u64().expect("fresh non-secret fixture name");
    let name = CString::new(format!("storage-{random:016x}")).expect("one terminated component");
    Scratch::create(approved_parent(), name).expect("exclusive private APFS fixture")
}

fn seed(scratch: &Scratch, name: &CStr, bytes: &[u8]) -> File {
    let mut file = scratch.stage(name).expect("private staged file");
    file.write_all(bytes).expect("write synthetic bytes");
    file
}

fn contents(mut file: &File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.rewind().expect("fixture file rewind");
    file.read_to_end(&mut bytes).expect("fixture file bytes");
    bytes
}

fn reopened(scratch: &Scratch, name: &CStr) -> File {
    open_at(&scratch.directory, name, libc::O_RDONLY, Stage::Destination)
        .expect("independent reopen")
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn publishes_and_reopens_on_owned_apfs() {
    let mut scratch = fixture();
    let old = seed(&scratch, c"current", b"initial bytes");
    let staged = seed(&scratch, c"next", b"replacement bytes");
    let sentinel = seed(&scratch, c"sentinel", b"unrelated bytes");
    assert_eq!(scratch.replace(c"next", c"current"), Publication::Completed);
    assert_eq!(
        contents(&reopened(&scratch, c"current")),
        b"replacement bytes"
    );
    assert_eq!(contents(&old), b"initial bytes");
    assert_eq!(
        named_stat(&scratch.directory, c"next", Stage::Source).err(),
        Some(Fault::io(
            Stage::Source,
            &io::Error::from_raw_os_error(libc::ENOENT)
        ))
    );
    assert_eq!(
        scratch.remove(c"current", &old),
        Err(Fault::new(Stage::Cleanup, ErrorKind::PermissionDenied))
    );
    assert_eq!(
        scratch.remove(c"current", &sentinel),
        Err(Fault::new(Stage::Cleanup, ErrorKind::InvalidData))
    );
    assert_eq!(
        contents(&reopened(&scratch, c"current")),
        b"replacement bytes"
    );
    scratch
        .remove(c"current", &staged)
        .expect("remove only the staged inode");
    scratch
        .remove(c"sentinel", &sentinel)
        .expect("remove owned sentinel");
    scratch.finish().expect("remove empty owned root");
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn refuses_collisions_links_and_non_siblings() {
    let mut scratch = fixture();
    let mutates = Scratch::create(
        scratch.parent.try_clone().expect("retained parent"),
        scratch.name.clone(),
    );
    assert_eq!(
        mutates.err(),
        Some(Fault::io(
            Stage::Root,
            &io::Error::from_raw_os_error(libc::EEXIST)
        ))
    );
    let current = seed(&scratch, c"current", b"old");
    let next = seed(&scratch, c"next", b"new");
    assert_eq!(
        scratch.stage(c"current").err(),
        Some(Fault::io(
            Stage::Create,
            &io::Error::from_raw_os_error(libc::EEXIST)
        ))
    );
    for name in [c"", c".", c"..", c"../next", c"/next", c"sub/next"] {
        assert_eq!(
            scratch.replace(name, c"current"),
            Publication::NotPublished(Fault::new(Stage::Source, ErrorKind::InvalidInput))
        );
    }
    assert_eq!(
        scratch.replace(c"next", c"missing"),
        Publication::NotPublished(Fault::io(
            Stage::Destination,
            &io::Error::from_raw_os_error(libc::ENOENT)
        ))
    );
    assert_eq!(
        scratch.replace(c"current", c"current"),
        Publication::NotPublished(Fault::new(Stage::Destination, ErrorKind::InvalidInput))
    );
    let dirfd = scratch.directory.as_raw_fd();
    // SAFETY: The fixed relative target and link are both under this held fixture.
    let symlink = unsafe { libc::symlinkat(c"next".as_ptr(), dirfd, c"link".as_ptr()) };
    assert_eq!(symlink, 0, "create a symlink inside the owned root");
    let link = named_stat(&scratch.directory, c"link", Stage::Cleanup).expect("link identity");
    assert_eq!(
        scratch.replace(c"link", c"current"),
        Publication::NotPublished(Fault::new(Stage::Source, ErrorKind::PermissionDenied))
    );
    assert_eq!(
        scratch.replace(c"next", c"link"),
        Publication::NotPublished(Fault::new(Stage::Destination, ErrorKind::PermissionDenied))
    );
    assert_eq!(
        scratch.stage(c"link").err().map(|error| error.errno),
        Some(Some(libc::EEXIST))
    );
    unlink(&scratch.directory, c"link", &link, 0).expect("remove only the owned symlink");
    for (name, stage) in [(c"current", Stage::Destination), (c"next", Stage::Source)] {
        // SAFETY: The fixed alias and its source are siblings in the held test directory.
        let linked = unsafe { libc::linkat(dirfd, name.as_ptr(), dirfd, c"alias".as_ptr(), 0) };
        assert_eq!(linked, 0, "create an alias inside the owned root");
        let linked =
            named_stat(&scratch.directory, name, Stage::Cleanup).expect("owned hard link identity");
        assert_eq!(linked.st_nlink, 2);
        assert_eq!(
            scratch.replace(c"next", c"current"),
            Publication::NotPublished(Fault::new(stage, ErrorKind::PermissionDenied))
        );
        unlink(&scratch.directory, c"alias", &linked, 0).expect("remove only the fixture alias");
    }
    assert_eq!(contents(&reopened(&scratch, c"current")), b"old");
    assert_eq!(contents(&reopened(&scratch, c"next")), b"new");
    scratch.remove(c"next", &next).expect("remove owned stage");
    scratch
        .remove(c"current", &current)
        .expect("remove owned current");
    scratch.finish().expect("remove empty owned root");
}

// SAFETY: Signatures from Darwin sys/acl.h; the fixture releases each allocated ACL.
unsafe extern "C" {
    fn acl_from_text(text: *const libc::c_char) -> *mut libc::c_void;
    fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
    fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
}

fn install_acl(file: &File, entry: &CStr) {
    // SAFETY: entry is terminated and contains a synthetic numeric UUID and no account name.
    let acl = NonNull::new(unsafe { acl_from_text(entry.as_ptr()) }).expect("valid synthetic ACL");
    // SAFETY: Both the fd and ACL are live and belong to this fixture.
    let installed = unsafe { acl_set_fd(file.as_raw_fd(), acl.as_ptr()) };
    // SAFETY: This is the sole owner and frees the ACL once, including if installation failed.
    let freed = unsafe { acl_free(acl.as_ptr()) };
    assert_eq!(installed, 0, "install ACL only on the owned fixture");
    assert_eq!(freed, 0, "release ACL allocation");
}

#[test]
fn only_a_completed_zero_acl_query_is_absence() {
    assert_eq!(acl_absent(Ok(()), 0, Stage::Root), Ok(()));
    assert_eq!(
        acl_absent(Ok(()), -1, Stage::Root),
        Err(Fault::new(Stage::Root, ErrorKind::InvalidData))
    );
    for presence in [1, 32, libc::c_int::MAX] {
        assert_eq!(
            acl_absent(Ok(()), presence, Stage::Root),
            Err(Fault::new(Stage::Root, ErrorKind::PermissionDenied))
        );
    }
    let error = io::Error::from_raw_os_error(libc::EIO);
    assert_eq!(
        acl_absent(Err(io::Error::from_raw_os_error(libc::EIO)), 0, Stage::Root),
        Err(Fault::io(Stage::Root, &error))
    );
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn refuses_parent_mode_and_even_deny_only_acl() {
    let scratch = fixture();
    let try_child = || {
        Scratch::create(
            scratch.directory.try_clone().expect("held fixture root"),
            c"would-be-child".to_owned(),
        )
    };
    scratch
        .directory
        .set_permissions(std::fs::Permissions::from_mode(0o720))
        .expect("set unsafe fixture mode");
    assert_eq!(
        try_child().err(),
        Some(Fault::new(Stage::Parent, ErrorKind::PermissionDenied))
    );
    scratch
        .directory
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .expect("restore fixture mode");
    let root =
        named_stat(&scratch.parent, &scratch.name, Stage::Root).expect("record fixture root");
    install_acl(
        &scratch.directory,
        c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n",
    );
    assert_eq!(
        try_child().err(),
        Some(Fault::new(Stage::Parent, ErrorKind::PermissionDenied))
    );
    assert_eq!(
        named_stat(&scratch.directory, c"would-be-child", Stage::Root).err(),
        Some(Fault::io(
            Stage::Root,
            &io::Error::from_raw_os_error(libc::ENOENT)
        ))
    );
    unlink(&scratch.parent, &scratch.name, &root, libc::AT_REMOVEDIR)
        .expect("fixture owner removes only its recorded empty ACL test root");
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn indeterminate_rename_or_sync_retains_data_and_stops_the_owner() {
    for stage in [Stage::Rename, Stage::DirectorySync] {
        let mut scratch = fixture();
        let old = seed(&scratch, c"current", b"old bytes");
        let staged = seed(&scratch, c"next", b"new bytes");
        let outcome = scratch.replace_with(
            c"next",
            c"current",
            |directory, from, to| {
                rename(directory, from, to)?;
                if stage == Stage::Rename {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            },
            |_| Err(io::Error::from_raw_os_error(libc::EIO)),
        );
        assert_eq!(
            outcome,
            Publication::Indeterminate(Fault::io(stage, &io::Error::from_raw_os_error(libc::EIO)))
        );
        let stopped = Fault::new(Stage::Stopped, ErrorKind::Other);
        assert_eq!(
            scratch.replace(c"current", c"next"),
            Publication::NotPublished(stopped)
        );
        assert_eq!(scratch.stage(c"another").err(), Some(stopped));
        assert_eq!(scratch.remove(c"current", &staged), Err(stopped));
        assert_eq!(contents(&reopened(&scratch, c"current")), b"new bytes");
        assert_eq!(contents(&old), b"old bytes");
        assert_eq!(scratch.finish(), Err(stopped));
    }
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn reports_available_capacity_with_checked_units() {
    let scratch = fixture();
    check_volume(&scratch.directory).expect("local writable APFS with ownership");
    let mut sample = capacity(&scratch.directory).expect("held directory capacity");
    assert!(
        u128::from(scratch.free_space().expect("caller-available bytes"))
            <= u128::from(sample.f_blocks) * u128::from(sample.f_frsize),
        "available bytes can't exceed total space on the admitted volume"
    );
    sample.f_blocks = 10;
    sample.f_bfree = 7;
    sample.f_bavail = 3;
    sample.f_frsize = 4096;
    assert_eq!(available_bytes(&sample), Ok(12_288));
    let invalid = Err(Fault::new(Stage::Capacity, ErrorKind::InvalidData));
    sample.f_frsize = 0;
    assert_eq!(available_bytes(&sample), invalid);
    sample.f_frsize = u64::MAX;
    sample.f_blocks = u32::MAX;
    sample.f_bavail = u32::MAX;
    assert_eq!(available_bytes(&sample), invalid);
    scratch.finish().expect("remove empty owned root");
}

#[test]
fn darwin_link_errors_are_classified_by_raw_errno() {
    assert_eq!((libc::ENOTSUP, libc::EOPNOTSUPP), (45, 102));
    for errno in [libc::ENOTSUP, libc::EOPNOTSUPP] {
        assert!(links_unsupported(&io::Error::from_raw_os_error(errno)));
    }
    for errno in [
        libc::EPERM,
        libc::EACCES,
        libc::EIO,
        libc::EXDEV,
        libc::EEXIST,
    ] {
        assert!(!links_unsupported(&io::Error::from_raw_os_error(errno)));
    }
    assert!(!links_unsupported(&io::Error::from(ErrorKind::Unsupported)));
}
