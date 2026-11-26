use super::*;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::FromRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use crate::platform::macos::protected::tests::{Scratch, install_acl};
use crate::platform::macos::protected::{open_directory, stat};

const DENY: &CStr = c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n";
const CONFIG: &CStr = c"config.toml";
const TEMP: &CStr = c"config.toml.tmp";

fn anchor(scratch: &Scratch) -> PathBuf {
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    PathBuf::from(parent).join(OsStr::from_bytes(scratch.residual.to_bytes()))
}

fn mode(folder: &File, name: &CStr) -> (libc::mode_t, libc::nlink_t) {
    let found = entry_stat(folder, name).expect("fixture entry");
    (found.st_mode & 0o7777, found.st_nlink)
}

fn absent(folder: &File, name: &CStr) -> bool {
    entry_stat(folder, name).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn read_all(file: &File) -> Vec<u8> {
    let mut bytes = Vec::new();
    (&*file).read_to_end(&mut bytes).expect("read fixture file");
    bytes
}

fn put(scratch: &mut Scratch, folder: &File, name: &CStr, mode: libc::mode_t) -> File {
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: folder is a held fixture folder and name one fixed component; creation is exclusive.
    let fd = unsafe { libc::openat(folder.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    assert!(fd >= 0, "exclusive fixture file");
    // SAFETY: the successful open transferred a fresh, uniquely owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    scratch.record(folder, name).expect("record fixture file");
    (&file).write_all(b"fixture").expect("fixture bytes");
    // SAFETY: file is the fixture just created; the mode is a fixed fixture value.
    let changed = unsafe { libc::fchmod(file.as_raw_fd(), mode) };
    assert_eq!(changed, 0, "fixture mode");
    file
}

/// A crash leftover at the reserved leaf, which the next save removes, so it isn't recorded.
fn leftover(folder: &File) {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: folder is a held fixture folder and the leaf fixed; creation is exclusive.
    let fd = unsafe { libc::openat(folder.as_raw_fd(), TEMP.as_ptr(), flags, 0o644) };
    assert!(fd >= 0, "exclusive crash leftover");
    // SAFETY: the successful open transferred a fresh, uniquely owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    (&file).write_all(b"junk").expect("leftover bytes");
}

fn folder_mode(_: &mut Scratch, folder: &File) {
    // SAFETY: folder is the held fixture product folder; the mode is a fixed fixture value.
    let changed = unsafe { libc::fchmod(folder.as_raw_fd(), 0o755) };
    assert_eq!(changed, 0, "fixture folder mode");
}

fn folder_acl(_: &mut Scratch, folder: &File) {
    install_acl(folder, DENY).expect("install fixed UUID deny fixture");
}

fn file_mode(scratch: &mut Scratch, folder: &File) {
    drop(put(scratch, folder, CONFIG, 0o644));
}

fn file_acl(scratch: &mut Scratch, folder: &File) {
    install_acl(&put(scratch, folder, CONFIG, 0o600), DENY).expect("install file ACL fixture");
}

fn file_names(scratch: &mut Scratch, folder: &File) {
    drop(put(scratch, folder, CONFIG, 0o600));
    let fd = folder.as_raw_fd();
    // SAFETY: folder is held; both names are fixed fixture components.
    let linked = unsafe { libc::linkat(fd, CONFIG.as_ptr(), fd, c"second".as_ptr(), 0) };
    assert_eq!(linked, 0, "fixture second name");
    scratch
        .record(folder, c"second")
        .expect("record second name");
}

fn file_link(scratch: &mut Scratch, folder: &File) {
    // SAFETY: folder is held; both names are fixed and terminated.
    let made =
        unsafe { libc::symlinkat(c"elsewhere".as_ptr(), folder.as_raw_fd(), CONFIG.as_ptr()) };
    assert_eq!(made, 0, "fixture link");
    scratch.record(folder, CONFIG).expect("record link");
}

fn file_fifo(scratch: &mut Scratch, folder: &File) {
    // SAFETY: folder is held and the name fixed; creation is exclusive.
    let made = unsafe { libc::mkfifoat(folder.as_raw_fd(), CONFIG.as_ptr(), 0o600) };
    assert_eq!(made, 0, "fixture FIFO");
    scratch.record(folder, CONFIG).expect("record FIFO");
}

fn file_folder(scratch: &mut Scratch, folder: &File) {
    // SAFETY: folder is held and the name fixed; creation is exclusive.
    let made = unsafe { libc::mkdirat(folder.as_raw_fd(), CONFIG.as_ptr(), 0o700) };
    assert_eq!(made, 0, "fixture folder at the settings name");
    scratch.record(folder, CONFIG).expect("record folder");
}

/// Records what the saves made, the folder first, so cleanup removes the file before it.
fn record_saved(scratch: &mut Scratch, product: &CStr) {
    let root = scratch.root.try_clone().expect("retain scratch root");
    scratch
        .record(&root, product)
        .expect("record product folder");
    let folder = open_directory(root.as_raw_fd(), product).expect("product folder");
    scratch.record(&folder, CONFIG).expect("record settings");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_a_first_save_makes_private_settings_and_later_saves_replace_them() {
    let mut scratch = Scratch::new();
    let support = anchor(&scratch);
    assert_eq!(read_settings(&support, "product"), Ok(None));
    drop(scratch.directory(c"empty"));
    assert_eq!(
        read_settings(&support, "empty"),
        Ok(None),
        "a private product folder without the file"
    );
    assert_eq!(
        read_settings(&support.join("absent"), "product"),
        Err(AcquisitionError::Open(io::ErrorKind::NotFound)),
        "a missing anchor refuses rather than reading as no settings"
    );
    assert_eq!(replace_settings(&support, "product", b"first"), Ok(()));
    let folder = open_directory(scratch.root.as_raw_fd(), c"product").expect("product folder");
    assert_eq!(
        stat(folder.as_raw_fd()).expect("folder").st_mode & 0o7777,
        0o700
    );
    assert_eq!(mode(&folder, CONFIG), (0o600, 1));
    assert!(absent(&folder, TEMP), "no staged copy is left");
    assert_eq!(
        read_settings(&support, "product"),
        Ok(Some(b"first".to_vec()))
    );

    let reader = open_at(folder.as_raw_fd(), CONFIG, READ_FLAGS, 0).expect("old reader");
    assert_eq!(replace_settings(&support, "product", b"second"), Ok(()));
    assert_eq!(
        read_settings(&support, "product"),
        Ok(Some(b"second".to_vec()))
    );
    assert_eq!(
        read_all(&reader),
        b"first",
        "a held reader keeps the old file"
    );

    leftover(&folder);
    assert_eq!(replace_settings(&support, "product", b"third"), Ok(()));
    assert_eq!(
        read_settings(&support, "product"),
        Ok(Some(b"third".to_vec()))
    );
    assert!(
        absent(&folder, TEMP),
        "the crash leftover was replaced unread"
    );

    // SAFETY: folder is held and the reserved leaf is a fixed fixture name; creation is exclusive.
    let made = unsafe { libc::mkdirat(folder.as_raw_fd(), TEMP.as_ptr(), 0o700) };
    assert_eq!(made, 0, "fixture folder at the reserved leaf");
    let blocked = replace_settings(&support, "product", b"fourth");
    let kept = read_settings(&support, "product");
    // SAFETY: folder is held and the empty fixture folder at the reserved leaf is removed once.
    let removed = unsafe { libc::unlinkat(folder.as_raw_fd(), TEMP.as_ptr(), libc::AT_REMOVEDIR) };
    assert_eq!(removed, 0, "remove the reserved-leaf fixture");
    drop((reader, folder));
    record_saved(&mut scratch, c"product");
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
    assert_eq!(
        (blocked, kept),
        (
            Err(SettingsWriteError::Unchanged(AcquisitionError::Open(
                io::ErrorKind::PermissionDenied
            ))),
            Ok(Some(b"third".to_vec()))
        ),
        "a folder at the reserved leaf is never removed"
    );
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_settings_refuse_what_isnt_private() {
    type Setup = fn(&mut Scratch, &File);
    let cases: [(&CStr, Setup, Untrusted); 8] = [
        (c"folder-mode", folder_mode, Untrusted::Unsafe),
        (c"folder-acl", folder_acl, Untrusted::Unsafe),
        (c"file-mode", file_mode, Untrusted::Unsafe),
        (c"file-acl", file_acl, Untrusted::Unsafe),
        (c"file-names", file_names, Untrusted::Unsafe),
        (c"file-link", file_link, Untrusted::Reparse),
        (c"file-fifo", file_fifo, Untrusted::Unsafe),
        (c"file-folder", file_folder, Untrusted::Unsafe),
    ];
    let mut scratch = Scratch::new();
    let support = anchor(&scratch);
    let mut outcomes = Vec::new();
    for (product, setup, _) in &cases {
        let folder = scratch.directory(product);
        setup(&mut scratch, &folder);
        let name = product.to_str().expect("ASCII fixture name");
        outcomes.push((
            read_settings(&support, name),
            replace_settings(&support, name, b"new"),
            absent(&folder, TEMP),
        ));
    }
    let root = scratch.root.try_clone().expect("retain scratch root");
    // SAFETY: root is held; both names are fixed and terminated.
    let made =
        unsafe { libc::symlinkat(c"file-mode".as_ptr(), root.as_raw_fd(), c"linked".as_ptr()) };
    assert_eq!(made, 0, "fixture product link");
    scratch
        .record(&root, c"linked")
        .expect("record linked product");
    let linked = (
        read_settings(&support, "linked"),
        replace_settings(&support, "linked", b"new"),
    );
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
    let expected: Vec<_> = cases
        .iter()
        .map(|(_, _, refusal)| {
            (
                Err(AcquisitionError::Admission(*refusal)),
                Err(SettingsWriteError::Unchanged(AcquisitionError::Admission(
                    *refusal,
                ))),
                true,
            )
        })
        .collect();
    assert_eq!(outcomes, expected);
    assert_eq!(
        linked,
        (
            Err(AcquisitionError::Admission(Untrusted::Unsafe)),
            Err(SettingsWriteError::Unchanged(AcquisitionError::Admission(
                Untrusted::Unsafe
            )))
        ),
        "a linked product folder fails its no-follow directory open"
    );
}

fn on_hook(hook: impl FnOnce() + 'static) {
    HOOK.set(Some(Box::new(hook)));
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_a_replacement_reports_only_what_it_can_prove() {
    let mut scratch = Scratch::new();
    let support = anchor(&scratch);
    assert_eq!(replace_settings(&support, "product", b"old"), Ok(()));
    let faults: [(&[Fault], &[u8]); 5] = [
        (&[Fault::AnchorSync], b"anchor"),
        (&[Fault::RenameBefore], b"before"),
        (&[Fault::RenameAfter], b"after1"),
        (&[Fault::FolderSync], b"folder"),
        (&[Fault::RenameAfter, Fault::FolderSync], b"after2"),
    ];
    let outcomes: Vec<_> = faults
        .into_iter()
        .map(|(faults, text)| {
            FAULTS.set(faults.to_vec());
            let written = replace_settings(&support, "product", text);
            let left = FAULTS.take();
            (written, left, read_settings(&support, "product"))
        })
        .collect();
    let after = replace_settings(&support, "product", b"last");
    let read = read_settings(&support, "product");
    record_saved(&mut scratch, c"product");
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
    let unchanged = |kind| Err(SettingsWriteError::Unchanged(AcquisitionError::Open(kind)));
    let unconfirmed = Err(SettingsWriteError::Unconfirmed);
    assert_eq!(
        outcomes,
        [
            (
                unchanged(io::ErrorKind::Other),
                vec![],
                Ok(Some(b"old".to_vec()))
            ),
            (
                unchanged(io::ErrorKind::ResourceBusy),
                vec![],
                Ok(Some(b"old".to_vec()))
            ),
            (Ok(()), vec![], Ok(Some(b"after1".to_vec()))),
            (unconfirmed, vec![], Ok(Some(b"folder".to_vec()))),
            (unconfirmed, vec![], Ok(Some(b"after2".to_vec()))),
        ]
    );
    assert_eq!((after, read), (Ok(()), Ok(Some(b"last".to_vec()))));
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_the_last_checks_see_an_acl_or_a_moved_folder() {
    let mut scratch = Scratch::new();
    let support = anchor(&scratch);
    let root = scratch.root.try_clone().expect("retain scratch root");
    for product in [c"read-acl", c"write-acl", c"moved"] {
        let name = product.to_str().expect("ASCII fixture name");
        assert_eq!(replace_settings(&support, name, b"old"), Ok(()));
    }
    let folder = |name: &CStr| open_directory(root.as_raw_fd(), name).expect("product folder");

    let file = open_at(folder(c"read-acl").as_raw_fd(), CONFIG, READ_FLAGS, 0).expect("settings");
    on_hook(move || install_acl(&file, DENY).expect("file ACL after the look"));
    let read = read_settings(&support, "read-acl");

    let written = folder(c"write-acl");
    on_hook(move || install_acl(&written, DENY).expect("folder ACL before the last checks"));
    let replaced = replace_settings(&support, "write-acl", b"new");

    let held = root.try_clone().expect("retain scratch root");
    on_hook(move || {
        let fd = held.as_raw_fd();
        // SAFETY: held is the scratch root; both names are fixed fixture components.
        let moved = unsafe { libc::renameat(fd, c"moved".as_ptr(), fd, c"elsewhere".as_ptr()) };
        assert_eq!(moved, 0, "move the product folder at the rename failure");
    });
    FAULTS.set(vec![Fault::RenameBefore]);
    let detached = replace_settings(&support, "moved", b"new");

    for product in [c"read-acl", c"write-acl"] {
        record_saved(&mut scratch, product);
    }
    record_saved(&mut scratch, c"elsewhere");
    let moved = folder(c"elsewhere");
    scratch
        .record(&moved, TEMP)
        .expect("record the staged copy");
    let kept = read_all(&open_at(moved.as_raw_fd(), CONFIG, READ_FLAGS, 0).expect("old settings"));
    drop(moved);
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
    assert_eq!(read, Err(AcquisitionError::Admission(Untrusted::Unsafe)));
    assert_eq!(replaced, Err(SettingsWriteError::Unconfirmed));
    assert_eq!(
        (detached, kept),
        (Err(SettingsWriteError::Unconfirmed), b"old".to_vec()),
        "a folder moved off its path proves nothing unchanged"
    );
}
