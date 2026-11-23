use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read as _, Seek as _, Write as _};
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use super::custody::{acl_absent, stat};
use super::settings::{write_scratch_settings, write_scratch_settings_with};
use super::{
    Destination, Fault, Publication, RenameMode, Scratch, Stage, available_bytes,
    links_unsupported, named_stat, open_at, rename, sync_directory, unlink, volume,
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
    for stage in [Stage::Rename, Stage::DirectorySync, Stage::PublishedSync] {
        let mut scratch = fixture();
        let old = seed(&scratch, c"current", b"old bytes");
        let staged = seed(&scratch, c"next", b"new bytes");
        let outcome = scratch.replace_with(
            c"next",
            (c"current", Destination::Existing),
            &staged,
            |directory, from, to, mode| {
                rename(directory, from, to, mode)?;
                if stage == Stage::Rename {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            },
            |directory| {
                if stage == Stage::DirectorySync {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                sync_directory(directory)
            },
            |file| {
                if stage == Stage::PublishedSync {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                file.sync_all()
            },
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
    let mut sample = volume(&scratch.directory, Stage::Capacity).expect("held APFS capacity");
    assert!(
        u128::from(scratch.free_space().expect("caller-available bytes"))
            <= u128::from(sample.f_blocks) * u128::from(sample.f_bsize),
        "available bytes can't exceed total space on the admitted volume"
    );
    sample.f_blocks = 10;
    sample.f_bfree = 7;
    sample.f_bavail = 3;
    sample.f_bsize = 4096;
    sample.f_iosize = 1_048_576;
    assert_eq!(available_bytes(&sample), Ok(12_288));
    sample.f_blocks = (1 << 32) + 10;
    sample.f_bfree = (1 << 32) + 7;
    sample.f_bavail = (1 << 32) + 3;
    assert_eq!(available_bytes(&sample), Ok((16 << 40) + 12_288));
    let invalid = Err(Fault::new(Stage::Capacity, ErrorKind::InvalidData));
    sample.f_bsize = 0;
    assert_eq!(available_bytes(&sample), invalid);
    sample.f_bsize = 4096;
    sample.f_blocks = u64::MAX;
    sample.f_bavail = u64::MAX;
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

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn settings_route_publishes_only_beneath_the_owned_root() {
    let scratch = fixture();
    let outside = fixture();
    let base = std::path::PathBuf::from(
        std::env::var_os("MAC_SCRATCH_TEST_PARENT").expect("explicit fixture parent"),
    );
    let root_path = base.join(scratch.name.to_str().expect("fixture name"));
    let outside_path = base.join(outside.name.to_str().expect("fixture name"));
    let sentinel = seed(&outside, c"settings", b"untouched");
    let unadmitted = base.with_file_name("not-admitted").join("settings");
    assert_eq!(
        write_scratch_settings(&unadmitted, b"wrong")
            .expect_err("outside the admitted root")
            .kind(),
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        write_scratch_settings(&root_path.join("case/../settings"), b"wrong")
            .expect_err("reject lexical traversal")
            .kind(),
        ErrorKind::InvalidInput
    );
    std::os::unix::fs::symlink(&outside_path, root_path.join("alias")).expect("fixture symlink");
    assert_eq!(
        write_scratch_settings(&root_path.join("alias/settings"), b"wrong")
            .expect_err("don't follow a directory link")
            .raw_os_error(),
        Some(libc::ENOTDIR)
    );
    assert_eq!(contents(&sentinel), b"untouched");
    let path = root_path.join("case/settings");
    write_scratch_settings(&path, b"first").expect("first publication");
    assert_eq!(std::fs::read(&path).expect("read first save"), b"first");
    write_scratch_settings(&path, b"replacement").expect("second publication");
    assert_eq!(
        std::fs::read(&path).expect("read replacement"),
        b"replacement"
    );
    let parent = open_at(
        &scratch.directory,
        c"case",
        libc::O_RDONLY | libc::O_DIRECTORY,
        Stage::Parent,
    )
    .expect("hold test case directory");
    let occupied = open_at(
        &parent,
        c"settings.tmp",
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        Stage::Create,
    )
    .expect("preoccupy the staging name");
    (&occupied)
        .write_all(b"leftover bytes")
        .expect("seed the occupied stage");
    assert_eq!(
        write_scratch_settings(&path, b"wrong")
            .expect_err("no temp reuse")
            .raw_os_error(),
        Some(libc::EEXIST)
    );
    assert_eq!(contents(&occupied), b"leftover bytes");
    assert_eq!(
        std::fs::read(&path).expect("read unchanged settings"),
        b"replacement"
    );
    let occupied = named_stat(&parent, c"settings.tmp", Stage::Cleanup).expect("occupied identity");
    unlink(&parent, c"settings.tmp", &occupied, 0).expect("remove only fixture temp");
    let settings = named_stat(&parent, c"settings", Stage::Cleanup).expect("new published inode");
    unlink(&parent, c"settings", &settings, 0).expect("remove only published entry");
    let case = named_stat(&scratch.directory, c"case", Stage::Cleanup).expect("case identity");
    unlink(&scratch.directory, c"case", &case, libc::AT_REMOVEDIR).expect("empty case");
    let link = named_stat(&scratch.directory, c"alias", Stage::Cleanup).expect("alias identity");
    unlink(&scratch.directory, c"alias", &link, 0).expect("remove fixture link");
    scratch.finish().expect("empty settings fixture");
    outside
        .remove(c"settings", &sentinel)
        .expect("remove untouched sentinel");
    outside.finish().expect("empty outside fixture");
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn staged_name_substitution_refuses_the_wrong_inode() {
    let mut scratch = fixture();
    let old = seed(&scratch, c"current", b"old bytes");
    let staged = seed(&scratch, c"next", b"intended bytes");
    rename(
        &scratch.directory,
        c"next",
        c"displaced",
        RenameMode::Replace,
    )
    .expect("move the staged name");
    let substitute = seed(&scratch, c"next", b"substitute bytes");
    assert_eq!(
        scratch.replace_with(
            c"next",
            (c"current", Destination::MayBeAbsent),
            &staged,
            rename,
            sync_directory,
            File::sync_all,
        ),
        Publication::NotPublished(Fault::new(Stage::Source, ErrorKind::InvalidData))
    );
    assert_eq!(contents(&reopened(&scratch, c"current")), b"old bytes");
    assert_eq!(contents(&reopened(&scratch, c"next")), b"substitute bytes");
    assert_eq!(contents(&staged), b"intended bytes");
    scratch.remove(c"current", &old).expect("remove fixture");
    scratch
        .remove(c"next", &substitute)
        .expect("remove fixture");
    scratch
        .remove(c"displaced", &staged)
        .expect("remove fixture");
    scratch.finish().expect("empty substitution fixture");
}

#[test]
#[ignore = "requires a private owned APFS fixture parent"]
fn first_save_does_not_replace_a_late_destination() {
    let mut scratch = fixture();
    let staged = seed(&scratch, c"next", b"first save bytes");
    assert_eq!(
        scratch.replace_with(
            c"next",
            (c"current", Destination::MayBeAbsent),
            &staged,
            |directory, from, to, mode| {
                let mut late = open_at(
                    directory,
                    to,
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    Stage::Create,
                )
                .expect("destination arrives after the absence sample");
                late.write_all(b"late bytes").expect("write late fixture");
                rename(directory, from, to, mode)
            },
            sync_directory,
            File::sync_all,
        ),
        Publication::NotPublished(Fault::io(
            Stage::Rename,
            &io::Error::from_raw_os_error(libc::EEXIST)
        ))
    );
    let late = reopened(&scratch, c"current");
    assert_eq!(contents(&late), b"late bytes");
    assert_eq!(contents(&reopened(&scratch, c"next")), b"first save bytes");
    scratch
        .remove(c"current", &late)
        .expect("remove late fixture");
    scratch
        .remove(c"next", &staged)
        .expect("remove fixture stage");
    scratch.finish().expect("empty first-save fixture");
}

fn settings_refusal(kind: ErrorKind) {
    let path = std::env::temp_dir().join("admission/settings");
    let error = write_scratch_settings(&path, b"unadmitted")
        .expect_err("subprocess must not be able to replace settings");
    assert_eq!(error.kind(), kind, "exact refusal at the settings boundary");
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<Fault>()),
        Some(&Fault::new(Stage::Root, kind)),
        "the writer must refuse at root admission, not later"
    );
    assert_eq!(
        std::fs::read(path).expect("read old payload"),
        b"old bytes",
        "a refused process must not change the named payload"
    );
}

#[test]
#[ignore = "requires a runner-seeded root with absent or inherited non-owner admission"]
fn settings_refuses_unowned_subprocess() {
    settings_refusal(ErrorKind::Unsupported);
}

#[test]
#[ignore = "requires runner-passed identity different from the seeded root"]
fn settings_refuses_mismatched_root() {
    settings_refusal(ErrorKind::InvalidData);
}

fn inherited_fixture_fd() -> RawFd {
    std::env::var("MAC_SCRATCH_TEST_FD")
        .expect("named inherited fixture descriptor")
        .parse()
        .expect("numeric fixture descriptor")
}

#[expect(
    clippy::disallowed_methods,
    reason = "spawns under the shared spawn lock"
)]
fn assert_exec_does_not_inherit_fixture() {
    let parent = stat(inherited_fixture_fd(), Stage::Root).expect("original fd is live in owner");
    let identity = format!("{}:{}", parent.st_dev, parent.st_ino);
    let output = {
        let _spawning = crate::platform::spawn_lock();
        std::process::Command::new(std::env::current_exe().expect("fixture executable"))
            .args([
                "--exact",
                "platform::fs::macos::tests::settings_exec_checks_no_fixture_handle",
                "--ignored",
            ])
            .env("MAC_SCRATCH_TEST_EXPECTED_CLOSED", identity)
            .output()
            .expect("exec fixture witness before any settings write")
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "exec witness must run and report no inherited root: {stdout}"
    );
}

#[test]
#[ignore = "requires a real runner-provisioned root before the first settings write"]
fn settings_exec_does_not_inherit_root_before_first_write() {
    assert_exec_does_not_inherit_fixture();
    let settings = std::env::temp_dir().join("handoff/settings");
    write_scratch_settings(&settings, b"owner bytes").expect("sealed owner can still publish");
    assert_eq!(
        std::fs::read(settings).expect("read owner bytes"),
        b"owner bytes"
    );
}

#[test]
#[ignore = "requires a runner-passed directory that fails root identity validation"]
fn settings_rejected_startup_does_not_leak_root_to_exec() {
    assert_exec_does_not_inherit_fixture();
    settings_refusal(ErrorKind::InvalidData);
}

#[test]
#[ignore = "runs only as the pre-write exec witness"]
fn settings_exec_checks_no_fixture_handle() {
    let identity = std::env::var("MAC_SCRATCH_TEST_EXPECTED_CLOSED").expect("expected identity");
    match stat(inherited_fixture_fd(), Stage::Root) {
        Ok(actual) => assert_ne!(
            format!("{}:{}", actual.st_dev, actual.st_ino),
            identity,
            "exec inherited the runner's directory handle"
        ),
        Err(error) => assert_eq!(
            error.errno,
            Some(libc::EBADF),
            "closed fd must report EBADF if its number was not reused"
        ),
    }
}

#[test]
#[ignore = "requires a supervised private APFS fixture and must exit 78"]
#[expect(
    clippy::disallowed_methods,
    reason = "spawns under the shared spawn lock"
)]
fn settings_uncertainty_exits_owner() {
    let path = std::env::temp_dir().join("admission/settings");
    write_scratch_settings(&path, b"old bytes").expect("admit owner and seed settings");
    let mut child = {
        let _spawning = crate::platform::spawn_lock();
        std::process::Command::new(std::env::current_exe().expect("fixture executable"))
            .args([
                "--exact",
                "platform::fs::macos::tests::settings_quarantine_child",
                "--ignored",
            ])
            .spawn()
            .expect("spawn an inherited non-owner in the owner's group")
    };
    let ready = std::env::temp_dir().join("quarantine-ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = ready.is_file().then(|| {
        writeln!(io::stderr(), "quarantine witness pid={}", child.id())
            .expect("send the synthetic witness PID to the supervisor");
        write_scratch_settings_with(&path, b"new bytes", |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        })
    });
    child.kill().expect("stop witness when owner did not exit");
    child.wait().expect("reap witness when owner did not exit");
    panic!("expected exit 78 after a ready witness, returned {result:?}");
}

#[test]
#[ignore = "runs only as the supervised owner's quarantine witness"]
fn settings_quarantine_child() {
    settings_refusal(ErrorKind::Unsupported);
    let ready = std::env::temp_dir().join("quarantine-ready");
    let _held = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(ready)
        .expect("signal non-owner refusal and keep the fixture open");
    std::thread::sleep(Duration::from_secs(30));
    panic!("runner did not stop the witness");
}
