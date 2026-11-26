use super::*;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use crate::platform::macos::protected::tests::{Scratch, install_acl};
use crate::platform::macos::protected::{classify_absent_acl, query_present_acl};

const DENY: &CStr = c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n";

type Listed = Box<dyn FnMut(&mut Vec<CString>)>;

thread_local! {
    static LISTED: RefCell<Option<Listed>> = RefCell::new(None);
}

/// Lets a test change a folder's names, or the namespace, right after the folder is listed.
pub(super) fn listed(names: &mut Vec<CString>) {
    LISTED.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(names);
        }
    });
}

fn on_listing(hook: impl FnMut(&mut Vec<CString>) + 'static) {
    LISTED.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

fn folder(scratch: &mut Scratch, parent: &File, name: &CStr, mode: libc::mode_t) -> File {
    // SAFETY: parent is a held fixture folder and name one fixed component; creation is exclusive.
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    assert_eq!(created, 0, "exclusive fixture folder");
    scratch.record(parent, name).expect("record fixture folder");
    let folder = open_directory(parent.as_raw_fd(), name).expect("open fixture folder");
    // SAFETY: folder is the fixture folder just opened; mode is a fixed fixture value.
    let changed = unsafe { libc::fchmod(folder.as_raw_fd(), mode) };
    assert_eq!(changed, 0, "fixture folder mode");
    folder
}

/// The access time every fixture file starts with: older than its modification time, so any read
/// of it (`read`, `pread` or an `mmap` fault) moves it on APFS.
const UNREAD: libc::time_t = 86_400;

/// A fixture file holding a few bytes, at `mode`, with an access time older than its change time.
fn file(scratch: &mut Scratch, parent: &File, name: &CStr, mode: libc::mode_t) -> File {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: parent is a held fixture folder and name one fixed component; creation is exclusive.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    assert!(fd >= 0, "exclusive fixture file");
    // SAFETY: the successful open transferred a fresh, uniquely owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    scratch.record(parent, name).expect("record fixture file");
    std::io::Write::write_all(&mut file, b"fixture").expect("fixture bytes");
    // SAFETY: file is the fixture just created; the mode is a fixed fixture value.
    let changed = unsafe { libc::fchmod(file.as_raw_fd(), mode) };
    assert_eq!(changed, 0, "fixture file mode");
    let times = [
        libc::timespec {
            tv_sec: UNREAD,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: 2 * UNREAD,
            tv_nsec: 0,
        },
    ];
    // SAFETY: file is the fixture just created and times holds the two timestamps futimens reads.
    let stamped = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
    assert_eq!(stamped, 0, "fixture file times");
    file
}

fn link(scratch: &mut Scratch, parent: &File, name: &CStr, target: &CStr) {
    // SAFETY: parent is a held fixture folder; both names are fixed and terminated.
    let made = unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) };
    assert_eq!(made, 0, "exclusive fixture link");
    scratch.record(parent, name).expect("record fixture link");
}

fn rename(parent: &File, from: &CStr, to: &CStr) {
    // SAFETY: parent is a held fixture folder; both names are fixed fixture components.
    let renamed = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    assert_eq!(renamed, 0, "fixture rename");
}

fn this_user() -> libc::uid_t {
    user().expect("tests run as an ordinary user")
}

#[test]
fn invalid_paths_refuse_before_anything_is_made() {
    for bytes in [
        b"".as_slice(),
        b"relative",
        b"/",
        b"//server/x",
        b"/a/../b",
        b"/a/./b",
        b"/a\0b",
    ] {
        let path = Path::new(OsStr::from_bytes(bytes));
        assert_eq!(
            owner_only_dir(path).err(),
            Some(Untrusted::Unsafe),
            "{bytes:?}"
        );
        assert_eq!(private_tree(path), Err(Untrusted::Unsafe), "{bytes:?}");
    }
}

fn absent_response(returned: u32) -> [u8; 60] {
    let mut bytes = [0_u8; 60];
    for (offset, value) in [
        (0, 60),
        (4, returned),
        (24, 7),
        (28, 2),
        (32, 501),
        (36, 20),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&u32::to_ne_bytes(value));
    }
    bytes[40..44].copy_from_slice(&0o700_u32.to_ne_bytes());
    bytes[44..52].copy_from_slice(&91_u64.to_ne_bytes());
    bytes
}

fn folder_sample() -> libc::stat {
    // SAFETY: zero is a valid starting value for libc's integer-only stat fields.
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    metadata.st_dev = 7;
    metadata.st_ino = 91;
    metadata.st_uid = 501;
    metadata.st_gid = 20;
    metadata.st_mode = libc::S_IFDIR | 0o700;
    metadata
}

#[test]
fn absence_needs_a_complete_query_that_leaves_the_acl_out() {
    let without = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_OWNERID
        | libc::ATTR_CMN_GRPID
        | libc::ATTR_CMN_ACCESSMASK
        | libc::ATTR_CMN_FILEID;
    let expected = folder_sample();
    assert_eq!(
        classify_absent_acl(0, &absent_response(without), &expected),
        Ok(())
    );
    assert_eq!(
        classify_absent_acl(
            0,
            &absent_response(without | libc::ATTR_CMN_EXTENDED_SECURITY),
            &expected
        ),
        Err(Untrusted::Unsafe),
        "a returned ACL, empty or not"
    );
    assert_eq!(
        classify_absent_acl(-1, &absent_response(without), &expected),
        Err(Untrusted::Io)
    );
    assert_eq!(
        classify_absent_acl(
            0,
            &absent_response(without & !libc::ATTR_CMN_FILEID),
            &expected
        ),
        Err(Untrusted::Io),
        "an incomplete query"
    );
    let mut other = absent_response(without);
    other[16..20].copy_from_slice(&1_u32.to_ne_bytes());
    assert_eq!(
        classify_absent_acl(0, &other, &expected),
        Err(Untrusted::Io)
    );
    let mut short = absent_response(without);
    short[0..4].copy_from_slice(&51_u32.to_ne_bytes());
    assert_eq!(
        classify_absent_acl(0, &short, &expected),
        Err(Untrusted::Io)
    );
    let mut moved = expected;
    moved.st_ino += 1;
    assert_eq!(
        classify_absent_acl(0, &absent_response(without), &moved),
        Err(Untrusted::Io),
        "another object's answer"
    );
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_public_paths_reach_the_named_entries() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    let base = Path::new(&parent).join(OsStr::from_bytes(scratch.residual.to_bytes()));

    let made = owner_only_dir(&base.join("public")).expect("creates through the public path");
    scratch
        .record(&root, c"public")
        .expect("record created folder");
    let named = look(&root, c"public").expect("the named entry");
    let held = stat(made.as_raw_fd()).expect("returned identity");
    assert_eq!((held.st_dev, held.st_ino), (named.st_dev, named.st_ino));
    assert_eq!(private_tree(&base.join("public")), Ok(()));
    assert_eq!(private_tree(&base.join("absent")), Err(Untrusted::Missing));

    drop(made);
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_owner_only_dir_creates_reuses_and_refuses() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let user = this_user();

    let made = owner_only_at(&root, c"made", user).expect("creates a private folder");
    scratch
        .record(&root, c"made")
        .expect("record created folder");
    let identity = stat(made.as_raw_fd()).expect("created identity");
    assert_eq!(identity.st_mode & libc::S_IFMT, libc::S_IFDIR);
    assert_eq!(identity.st_mode & 0o7777, 0o700);
    assert_eq!(identity.st_uid, user);
    let again = owner_only_at(&root, c"made", user).expect("reuses a private folder");
    assert!(same_inspection(
        &identity,
        &stat(again.as_raw_fd()).expect("reused identity")
    ));
    assert_eq!(
        owner_only_at(&root, c"made", user + 1).err(),
        Some(Untrusted::Unsafe),
        "another owner's folder"
    );

    drop(folder(&mut scratch, &root, c"open", 0o750));
    assert_eq!(
        owner_only_at(&root, c"open", user).err(),
        Some(Untrusted::Unsafe)
    );
    link(&mut scratch, &root, c"link", c"made");
    assert_eq!(
        owner_only_at(&root, c"link", user).err(),
        Some(Untrusted::Reparse)
    );
    drop(file(&mut scratch, &root, c"file", 0o600));
    assert_eq!(
        owner_only_at(&root, c"file", user).err(),
        Some(Untrusted::Unsafe)
    );
    let denied = folder(&mut scratch, &root, c"denied", 0o700);
    install_acl(&denied, DENY).expect("install fixed UUID deny fixture");
    assert_eq!(
        owner_only_at(&root, c"denied", user).err(),
        Some(Untrusted::Unsafe),
        "a DENY-only ACL is still an ACL"
    );

    drop((made, again, denied));
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_an_empty_present_acl_refuses() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let user = this_user();

    let empty = folder(&mut scratch, &root, c"empty", 0o700);
    install_acl(&empty, c"!#acl 1\n").expect("install an ACL with no entries");
    let (result, buffer) = query_present_acl(empty.as_raw_fd());
    assert_eq!(result, 0, "completed attribute query");
    let returned = u32::from_ne_bytes(buffer.0[4..8].try_into().expect("four bytes"));
    assert_ne!(
        returned & libc::ATTR_CMN_EXTENDED_SECURITY,
        0,
        "the fixture's ACL is present"
    );
    assert_eq!(
        owner_only_at(&root, c"empty", user).err(),
        Some(Untrusted::Unsafe)
    );
    assert_eq!(
        private_at(&root, c"empty", user, LIMITS),
        Err(Untrusted::Unsafe)
    );
    drop(folder(&mut scratch, &root, c"none", 0o700));
    assert!(owner_only_at(&root, c"none", user).is_ok(), "no ACL passes");

    drop(empty);
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_private_tree_passes_only_private_folders() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let user = this_user();

    let tree = folder(&mut scratch, &root, c"tree", 0o700);
    drop(folder(&mut scratch, &tree, c"sub", 0o700));
    assert_eq!(private_at(&root, c"tree", user, LIMITS), Ok(()));
    assert_eq!(
        private_at(&root, c"absent", user, LIMITS),
        Err(Untrusted::Missing)
    );

    let open = folder(&mut scratch, &root, c"open-root", 0o750);
    assert_eq!(
        private_at(&root, c"open-root", user, LIMITS),
        Err(Untrusted::Unsafe)
    );
    let cases: [(&CStr, Untrusted); 5] = [
        (c"open-file", Untrusted::Unsafe),
        (c"open-sub", Untrusted::Unsafe),
        (c"with-link", Untrusted::Reparse),
        (c"with-fifo", Untrusted::Unsafe),
        (c"denied-sub", Untrusted::Unsafe),
    ];
    let mut held = vec![open];
    for (name, _) in cases {
        let top = folder(&mut scratch, &root, name, 0o700);
        let sub = folder(&mut scratch, &top, c"sub", 0o700);
        held.push(top);
        held.push(sub);
    }
    drop(file(&mut scratch, &held[2], c"f", 0o644));
    // SAFETY: held[4] is the open-sub fixture's sub folder; the mode is a fixed fixture value.
    assert_eq!(unsafe { libc::fchmod(held[4].as_raw_fd(), 0o750) }, 0);
    link(&mut scratch, &held[6], c"l", c"f");
    // SAFETY: the with-fifo fixture folder is held; the name is a fixed fixture component.
    let fifo = unsafe { libc::mkfifoat(held[8].as_raw_fd(), c"p".as_ptr(), 0o600) };
    assert_eq!(fifo, 0, "exclusive fixture FIFO");
    scratch.record(&held[8], c"p").expect("record fixture FIFO");
    install_acl(&held[10], DENY).expect("install fixed UUID deny fixture");
    for (name, refused) in cases {
        assert_eq!(
            private_at(&root, name, user, LIMITS),
            Err(refused),
            "{name:?}"
        );
    }

    drop((tree, held));
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_private_tree_refuses_past_its_bounds() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let user = this_user();

    let deep = folder(&mut scratch, &root, c"deep", 0o700);
    let first = folder(&mut scratch, &deep, c"d1", 0o700);
    drop(folder(&mut scratch, &first, c"d2", 0o700));
    let at = |depth, entries| Limits { depth, entries };
    assert_eq!(private_at(&root, c"deep", user, at(2, 2)), Ok(()));
    assert_eq!(
        private_at(&root, c"deep", user, at(1, 2)),
        Err(Untrusted::Io)
    );
    assert_eq!(
        private_at(&root, c"deep", user, at(2, 1)),
        Err(Untrusted::Io)
    );

    let wide = folder(&mut scratch, &root, c"wide", 0o700);
    for name in [c"a", c"b", c"c"] {
        drop(folder(&mut scratch, &wide, name, 0o700));
    }
    assert_eq!(private_at(&root, c"wide", user, at(1, 3)), Ok(()));
    assert_eq!(
        private_at(&root, c"wide", user, at(1, 2)),
        Err(Untrusted::Io)
    );

    drop((deep, first, wide));
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_private_tree_skips_vanished_entries_and_refuses_a_replaced_folder() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let user = this_user();

    drop(folder(&mut scratch, &root, c"vanish", 0o700));
    on_listing(|names| names.push(c"gone".to_owned()));
    assert_eq!(private_at(&root, c"vanish", user, LIMITS), Ok(()));

    drop(folder(&mut scratch, &root, c"swap", 0o700));
    let parent = root.try_clone().expect("retain scratch root for the hook");
    on_listing(move |_| {
        rename(&parent, c"swap", c"swapped");
        // SAFETY: parent is the held scratch root; the name is a fixed fixture component.
        let made = unsafe { libc::mkdirat(parent.as_raw_fd(), c"swap".as_ptr(), 0o700) };
        assert_eq!(made, 0, "replacement fixture folder");
    });
    assert_eq!(
        private_at(&root, c"swap", user, LIMITS),
        Err(Untrusted::Unsafe),
        "a folder renamed during its walk"
    );
    LISTED.with(|slot| *slot.borrow_mut() = None);
    rename(&root, c"swap", c"swap-new");
    scratch
        .record(&root, c"swap-new")
        .expect("record replacement");
    rename(&root, c"swapped", c"swap");

    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_private_tree_checks_files_without_reading_them() {
    let mut scratch = Scratch::new();
    let root = scratch.root.try_clone().expect("retain scratch root");
    let user = this_user();
    let accessed = |parent: &File, name: &CStr| look(parent, name).expect("fixture look").st_atime;

    let tree = folder(&mut scratch, &root, c"tree", 0o700);
    drop(file(&mut scratch, &tree, c"secret", 0o600));
    let sub = folder(&mut scratch, &tree, c"sub", 0o700);
    drop(file(&mut scratch, &sub, c"inner", 0o600));
    assert_eq!(private_at(&root, c"tree", user, LIMITS), Ok(()));
    assert_eq!(
        accessed(&tree, c"secret"),
        UNREAD,
        "the walk never read the file"
    );
    assert_eq!(accessed(&sub, c"inner"), UNREAD, "nor the nested one");
    let found = look(&tree, c"secret").expect("fixture look");
    assert_eq!(
        private_file(&tree, c"secret", &found, user + 1),
        Err(Untrusted::Unsafe),
        "another owner's file"
    );

    let mut held = vec![tree, sub];
    for (name, mode) in [(c"group-file", 0o640), (c"read-only", 0o400)] {
        let top = folder(&mut scratch, &root, name, 0o700);
        drop(file(&mut scratch, &top, c"f", mode));
        assert_eq!(
            private_at(&root, name, user, LIMITS),
            Err(Untrusted::Unsafe),
            "{name:?}"
        );
        held.push(top);
    }
    let linked = folder(&mut scratch, &root, c"linked", 0o700);
    drop(file(&mut scratch, &linked, c"a", 0o600));
    // SAFETY: linked is a held fixture folder; both names are fixed fixture components.
    let made = unsafe {
        libc::linkat(
            linked.as_raw_fd(),
            c"a".as_ptr(),
            linked.as_raw_fd(),
            c"b".as_ptr(),
            0,
        )
    };
    assert_eq!(made, 0, "fixture hard link");
    scratch
        .record(&linked, c"b")
        .expect("record fixture hard link");
    assert_eq!(
        private_at(&root, c"linked", user, LIMITS),
        Err(Untrusted::Unsafe),
        "a second name"
    );
    held.push(linked);
    for (name, acl) in [(c"denied-file", DENY), (c"empty-acl-file", c"!#acl 1\n")] {
        let top = folder(&mut scratch, &root, name, 0o700);
        let fixture = file(&mut scratch, &top, c"f", 0o600);
        install_acl(&fixture, acl).expect("install fixture file ACL");
        assert_eq!(
            private_at(&root, name, user, LIMITS),
            Err(Untrusted::Unsafe),
            "{name:?}"
        );
        held.push(top);
    }

    drop(held);
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}
