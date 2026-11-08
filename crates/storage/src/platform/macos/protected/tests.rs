use super::*;
use std::ffi::OsStr;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

fn sample() -> libc::stat {
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
fn invalid_paths_are_refused_before_opening() {
    for bytes in [
        b"".as_slice(),
        b"relative",
        b"//server/path",
        b"/a/../b",
        b"/a/./b",
        b"/a\0b",
    ] {
        let path = Path::new(OsStr::from_bytes(bytes));
        assert_eq!(hold_chain(path).err(), Some(Untrusted::Unsafe), "{bytes:?}");
    }
    assert_eq!(components(Path::new("/")), Ok(Vec::new()));
    assert_eq!(
        components(Path::new("/one/two")),
        Ok(vec![c"one".to_owned(), c"two".to_owned()])
    );
}

#[test]
fn owner_and_mode_are_both_required() {
    let mut metadata = sample();
    assert_eq!(safe_directory(&metadata, 501), Ok(()));
    metadata.st_uid = 0;
    metadata.st_mode = libc::S_IFDIR | 0o755;
    assert_eq!(safe_directory(&metadata, 501), Ok(()));
    metadata.st_uid = 502;
    assert_eq!(safe_directory(&metadata, 501), Err(Untrusted::Unsafe));
    for owner in [0, 501] {
        for mode in [0o720, 0o702, 0o777, 0o1777] {
            metadata.st_uid = owner;
            metadata.st_mode = libc::S_IFDIR | mode;
            assert_eq!(safe_directory(&metadata, 501), Err(Untrusted::Unsafe));
        }
    }
    metadata.st_mode = libc::S_IFREG | 0o700;
    assert_eq!(safe_directory(&metadata, 501), Err(Untrusted::Unsafe));
}

#[test]
fn absence_requires_completed_explicit_output() {
    assert_eq!(acl_absent(0, 0), Ok(()));
    for result in [-1, 1] {
        for output in [-1, 0, 1, 32] {
            assert_eq!(acl_absent(result, output), Err(Untrusted::Io));
        }
    }
    assert_eq!(acl_absent(0, -1), Err(Untrusted::Io));
    for present in [1, 32, libc::c_int::MAX] {
        assert_eq!(acl_absent(0, present), Err(Untrusted::Unsafe));
    }
}

#[test]
fn every_inspected_identity_and_access_field_must_match() {
    let expected = sample();
    assert!(same_inspection(&expected, &sample()), "equal metadata");
    let changes: [fn(&mut libc::stat); 6] = [
        |m| m.st_dev += 1,
        |m| m.st_ino += 1,
        |m| m.st_uid += 1,
        |m| m.st_gid += 1,
        |m| m.st_mode |= libc::S_IWGRP,
        |m| m.st_mode = libc::S_IFREG | 0o700,
    ];
    for change in changes {
        let mut metadata = sample();
        change(&mut metadata);
        assert!(!same_inspection(&expected, &metadata), "metadata mismatch");
    }
}

fn put_word(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn acl_response(entries: &[(u32, u32)]) -> Vec<u8> {
    let payload_length = 44 + 24 * entries.len();
    let mut bytes = vec![0; 60 + payload_length];
    put_word(
        &mut bytes,
        0,
        u32::try_from(60 + payload_length).expect("small fixture"),
    );
    put_word(&mut bytes, 4, 0x8243_800a);
    put_word(&mut bytes, 24, 7);
    put_word(&mut bytes, 28, 2);
    put_word(&mut bytes, 32, 501);
    put_word(&mut bytes, 36, 20);
    put_word(&mut bytes, 40, 0o700);
    put_word(&mut bytes, 44, 16);
    put_word(
        &mut bytes,
        48,
        u32::try_from(payload_length).expect("small fixture"),
    );
    bytes[52..60].copy_from_slice(&91_u64.to_ne_bytes());
    bytes[60..96].fill(0xa5);
    put_word(
        &mut bytes,
        96,
        u32::try_from(entries.len()).expect("small fixture"),
    );
    for (index, &(flags, rights)) in entries.iter().enumerate() {
        bytes[104 + 24 * index..120 + 24 * index].fill(0xb6);
        put_word(&mut bytes, 120 + 24 * index, flags);
        put_word(&mut bytes, 124 + 24 * index, rights);
    }
    bytes
}

#[test]
fn completed_public_acl_bytes_admit_only_supported_denials() {
    for entries in [
        vec![],
        vec![(2, 0)],
        vec![(0x1f2, 0x0010_3ffe)],
        vec![(2, 4); 128],
    ] {
        let mut bytes = acl_response(&entries);
        assert_eq!(classify_present_acl(0, &bytes, &sample()), Ok(()));
        put_word(&mut bytes, 100, 1 << 17);
        assert_eq!(classify_present_acl(0, &bytes, &sample()), Ok(()));
    }
    let mut bytes = acl_response(&[]);
    let payload = bytes[60..].to_vec();
    bytes.resize(4096, 0xc7);
    bytes[4052..].copy_from_slice(&payload);
    put_word(&mut bytes, 0, 4096);
    put_word(&mut bytes, 44, 4008);
    assert_eq!(
        classify_present_acl(0, &bytes, &sample()),
        Ok(()),
        "aligned padding is not ACL authority"
    );
}

#[test]
fn public_acl_query_requires_completed_matching_attributes() {
    let bytes = acl_response(&[]);
    for result in [-1, 1, libc::c_int::MAX] {
        assert_eq!(
            classify_present_acl(result, &bytes, &sample()),
            Err(Untrusted::Io)
        );
    }
    assert_eq!(
        classify_present_acl(0, &[0; 4096], &sample()),
        Err(Untrusted::Io)
    );
    for length in 0..bytes.len() {
        assert_eq!(
            classify_present_acl(0, &bytes[..length], &sample()),
            Err(Untrusted::Io),
            "truncated byte {length}"
        );
    }
    for offset in [4, 8, 12, 16, 20] {
        for bit in 0..32 {
            let mut changed = bytes.clone();
            let initial = if offset == 4 { 0x8243_800a } else { 0 };
            put_word(&mut changed, offset, initial ^ (1 << bit));
            assert_eq!(
                classify_present_acl(0, &changed, &sample()),
                Err(Untrusted::Io),
                "returned group {offset}, bit {bit}"
            );
        }
    }
    let granting = acl_response(&[(1, 4)]);
    assert_eq!(
        classify_present_acl(-1, &granting, &sample()),
        Err(Untrusted::Io),
        "query failure precedes policy refusal"
    );
    assert_eq!(
        classify_present_acl(0, &bytes, &sample()),
        Ok(()),
        "a later valid response does not alter the failed result"
    );
}

#[test]
fn public_acl_reported_lengths_and_references_are_bounded() {
    for length in [0, 59, 103, 105, 4097, u32::MAX] {
        let mut bytes = acl_response(&[]);
        put_word(&mut bytes, 0, length);
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io)
        );
    }
    for offset in [
        0,
        4,
        12,
        17,
        20,
        0xffff_ffff,
        0xffff_ffd4,
        0x8000_0000,
        0x7fff_ffff,
    ] {
        let mut bytes = acl_response(&[]);
        put_word(&mut bytes, 44, offset);
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io),
            "reference {offset}"
        );
    }
    for length in [0, 40, 43, 45, 68, u32::MAX] {
        let mut bytes = acl_response(&[]);
        put_word(&mut bytes, 48, length);
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io)
        );
    }
    let mut trailing = acl_response(&[]);
    trailing.extend_from_slice(&[0; 4]);
    put_word(&mut trailing, 0, 108);
    assert_eq!(
        classify_present_acl(0, &trailing, &sample()),
        Err(Untrusted::Io),
        "unexplained trailing data"
    );
}

#[test]
fn public_acl_counted_payload_is_exact() {
    assert_eq!(
        classify_present_acl(0, &acl_response(&[(2, 4); 128]), &sample()),
        Ok(())
    );
    assert_eq!(
        classify_present_acl(0, &acl_response(&[(2, 4); 129]), &sample()),
        Err(Untrusted::Io)
    );
    for count in [1, 128, 129, u32::MAX] {
        let mut bytes = acl_response(&[]);
        put_word(&mut bytes, 96, count);
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io),
            "count {count}"
        );
    }
    let mut extra_entry = acl_response(&[(2, 4)]);
    put_word(&mut extra_entry, 96, 0);
    assert_eq!(
        classify_present_acl(0, &extra_entry, &sample()),
        Err(Untrusted::Io),
        "count does not hide appended entries"
    );
    for payload_length in [40_usize, 45, 48] {
        let mut bytes = acl_response(&[]);
        bytes.resize(60 + payload_length, 0);
        put_word(
            &mut bytes,
            0,
            u32::try_from(60 + payload_length).expect("small fixture"),
        );
        put_word(
            &mut bytes,
            48,
            u32::try_from(payload_length).expect("small fixture"),
        );
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io),
            "payload {payload_length}"
        );
    }
}

#[test]
fn public_acl_rejects_every_unsupported_word_bit() {
    for bit in 0..32 {
        let mut bytes = acl_response(&[(2, 0)]);
        if bit != 17 {
            put_word(&mut bytes, 100, 1 << bit);
            assert_eq!(
                classify_present_acl(0, &bytes, &sample()),
                Err(Untrusted::Unsafe),
                "ACL bit {bit}"
            );
        }
        if 0x1f2 & (1 << bit) == 0 {
            let mut bytes = acl_response(&[(2, 0)]);
            put_word(&mut bytes, 120, 2 | (1 << bit));
            assert_eq!(
                classify_present_acl(0, &bytes, &sample()),
                Err(Untrusted::Unsafe),
                "ACE bit {bit}"
            );
        }
        if 0x0010_3ffe & (1 << bit) == 0 {
            let mut bytes = acl_response(&[(2, 0)]);
            put_word(&mut bytes, 124, 1 << bit);
            assert_eq!(
                classify_present_acl(0, &bytes, &sample()),
                Err(Untrusted::Unsafe),
                "right bit {bit}"
            );
        }
    }
    for flag in [1, 1 << 16] {
        let mut bytes = acl_response(&[]);
        put_word(&mut bytes, 100, flag);
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Unsafe),
            "opaque bit 0 and raw deferred bit 16 both refuse"
        );
    }
    for kind in [0, 1, 3, 4, 15] {
        assert_eq!(
            classify_present_acl(0, &acl_response(&[(kind, 0)]), &sample()),
            Err(Untrusted::Unsafe)
        );
    }
}

#[test]
fn public_acl_binds_every_returned_identity_and_permission_field() {
    for (offset, value) in [
        (24, 8),
        (28, 1),
        (32, 0),
        (36, 21),
        (40, 0o720),
        (40, 0o600),
    ] {
        let mut bytes = acl_response(&[]);
        put_word(&mut bytes, offset, value);
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io),
            "binding field {offset}"
        );
    }
    let mut bytes = acl_response(&[]);
    bytes[52..60].copy_from_slice(&92_u64.to_ne_bytes());
    assert_eq!(
        classify_present_acl(0, &bytes, &sample()),
        Err(Untrusted::Io)
    );
    bytes[52..60].copy_from_slice(&(0x5b_u64 | (1 << 40)).to_ne_bytes());
    assert_eq!(
        classify_present_acl(0, &bytes, &sample()),
        Err(Untrusted::Io),
        "full 64-bit file identity"
    );
    let mut high_identity = sample();
    high_identity.st_ino = 0x5b | (1 << 40);
    assert_eq!(classify_present_acl(0, &bytes, &high_identity), Ok(()));
    let mut bytes = acl_response(&[]);
    put_word(&mut bytes, 40, 0o040_700);
    assert_eq!(
        classify_present_acl(0, &bytes, &sample()),
        Ok(()),
        "only ACCESSMASK permission bits are valid"
    );
    put_word(&mut bytes, 28, 1);
    assert_eq!(
        classify_present_acl(0, &bytes, &sample()),
        Err(Untrusted::Io),
        "ACCESSMASK cannot replace OBJTYPE"
    );
    let mut wrong_type = sample();
    wrong_type.st_mode = libc::S_IFREG | 0o700;
    assert_eq!(
        classify_present_acl(0, &acl_response(&[]), &wrong_type),
        Err(Untrusted::Io)
    );
}

#[test]
fn public_acl_rejects_any_allow_even_late_or_mixed() {
    for entries in [
        vec![(1, 0)],
        vec![(1, 2)],
        vec![(1, 4)],
        vec![(0x101, 0)],
        vec![(2, 4), (1, 0)],
        vec![(1, 0), (2, 4)],
    ] {
        assert_eq!(
            classify_present_acl(0, &acl_response(&entries), &sample()),
            Err(Untrusted::Unsafe)
        );
    }
    let mut entries = vec![(2, 4); 128];
    entries[127] = (1, 0);
    assert_eq!(
        classify_present_acl(0, &acl_response(&entries), &sample()),
        Err(Untrusted::Unsafe),
        "all counted entries must pass"
    );
    entries[127] = (2, 1 << 31);
    assert_eq!(
        classify_present_acl(0, &acl_response(&entries), &sample()),
        Err(Untrusted::Unsafe),
        "late unsupported right"
    );
}

unsafe extern "C" {
    fn acl_from_text(text: *const libc::c_char) -> *mut libc::c_void;
    fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
    fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
}

fn install_acl(file: &File, text: &CStr) -> Result<(), Untrusted> {
    // SAFETY: text is a terminated, fixed fixture ACL with a UUID, not an account lookup.
    let acl = NonNull::new(unsafe { acl_from_text(text.as_ptr()) }).ok_or(Untrusted::Io)?;
    // SAFETY: file is held and acl_from_text returned this live ACL.
    let inspection = if unsafe { acl_set_fd(file.as_raw_fd(), acl.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(Untrusted::Io)
    };
    // SAFETY: this function uniquely owns acl; no earlier return or retry after acquisition.
    let release = if unsafe { acl_free(acl.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(Untrusted::Io)
    };
    inspection.and(release)
}

struct Entry {
    parent: File,
    parent_identity: libc::stat,
    name: CString,
    identity: libc::stat,
}

struct Scratch {
    root: File,
    entries: Vec<Entry>,
    _ancestors: Vec<File>,
    residual: CString,
    user: libc::uid_t,
    cleaned: bool,
}

fn entry_stat(parent: &File, name: &CStr) -> Result<libc::stat, Untrusted> {
    let mut metadata = MaybeUninit::uninit();
    // SAFETY: parent is held, name is terminated, metadata uses libc's writable stat layout.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(Untrusted::Io);
    }
    // SAFETY: successful fstatat initialized metadata.
    Ok(unsafe { metadata.assume_init() })
}

impl Scratch {
    fn new() -> Self {
        let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
            .expect("native execution requires an explicitly approved fixture parent");
        let mut ancestors = crate::hold_chain(Path::new(&parent))
            .expect("admit the complete approved fixture-parent chain");
        let parent = ancestors.pop().expect("admitted chain contains its parent");
        // SAFETY: getuid has no arguments or ownership effects.
        let user = unsafe { libc::getuid() };
        let metadata = inspect(&parent, user).expect("approved parent passes actual inspection");
        assert_eq!(
            metadata.st_uid, user,
            "fixture parent must belong to this user"
        );
        let mut random = [0_u8; 16];
        // SAFETY: the array is writable for its full length; no data leaves the process.
        unsafe { libc::arc4random_buf(random.as_mut_ptr().cast(), random.len()) };
        let name = CString::new(format!("storage-a-{:032x}", u128::from_ne_bytes(random)))
            .expect("hex name has no NUL");
        // SAFETY: held parent and terminated fresh name; mkdirat refuses collisions.
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        assert_eq!(
            result, 0,
            "exclusive scratch creation failed; never adopt or delete a collision"
        );
        let root = open_directory(parent.as_raw_fd(), &name)
            .unwrap_or_else(|error| panic!("scratch residual {name:?}; opening failed: {error:?}"));
        let mut scratch = Self {
            root,
            entries: Vec::new(),
            _ancestors: ancestors,
            residual: name.clone(),
            user,
            cleaned: false,
        };
        scratch
            .record(&parent, &name)
            .expect("record owned scratch identity");
        assert!(
            same_inspection(
                &scratch.entries[0].identity,
                &stat(scratch.root.as_raw_fd()).expect("scratch descriptor identity")
            ),
            "created scratch matches its held descriptor"
        );
        scratch
    }

    fn record(&mut self, parent: &File, name: &CStr) -> Result<(), Untrusted> {
        let identity = entry_stat(parent, name)?;
        if identity.st_uid != self.user {
            return Err(Untrusted::Unsafe);
        }
        self.entries.push(Entry {
            parent: parent.try_clone().map_err(|_| Untrusted::Io)?,
            parent_identity: stat(parent.as_raw_fd())?,
            name: name.to_owned(),
            identity,
        });
        Ok(())
    }

    fn directory(&mut self, name: &CStr) -> File {
        // SAFETY: root is held; name is a fixed single fixture component; creation is exclusive.
        let result = unsafe { libc::mkdirat(self.root.as_raw_fd(), name.as_ptr(), 0o700) };
        assert_eq!(result, 0, "exclusive child creation");
        let root = self.root.try_clone().expect("retain scratch parent");
        self.record(&root, name).expect("record child identity");
        let child = open_directory(root.as_raw_fd(), name).expect("open fixture child");
        assert!(
            same_inspection(
                &entry_stat(&root, name).expect("entry stat"),
                &stat(child.as_raw_fd()).expect("descriptor stat")
            ),
            "created child identity"
        );
        child
    }

    fn walk(&self, path: &str) -> Result<Vec<File>, Untrusted> {
        let names = components(Path::new(path))?;
        hold_from(
            self.root.try_clone().expect("retain fixture root"),
            &names,
            self.user,
        )
    }

    fn cleanup(mut self) -> Result<(), Untrusted> {
        while let Some(entry) = self.entries.last() {
            let parent = stat(entry.parent.as_raw_fd())?;
            let current = entry_stat(&entry.parent, &entry.name)?;
            if !same_object(&parent, &entry.parent_identity)
                || !same_object(&current, &entry.identity)
                || current.st_uid != self.user
            {
                return Err(Untrusted::Unsafe);
            }
            let flags = if current.st_mode & libc::S_IFMT == libc::S_IFDIR {
                libc::AT_REMOVEDIR
            } else {
                0
            };
            // SAFETY: identities were checked, the parent remains held, no recursive deletion.
            if unsafe { libc::unlinkat(entry.parent.as_raw_fd(), entry.name.as_ptr(), flags) } != 0
            {
                return Err(Untrusted::Io);
            }
            self.entries.pop();
        }
        self.cleaned = true;
        Ok(())
    }
}

fn same_object(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_uid == right.st_uid
        && left.st_mode & libc::S_IFMT == right.st_mode & libc::S_IFMT
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.cleaned {
            eprintln!(
                "native fixture residual {:?}; cleanup not established",
                self.residual
            );
        }
    }
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_held_identity_access_and_symlink_refusal() {
    let mut scratch = Scratch::new();
    let safe = scratch.directory(c"safe");
    let held = scratch
        .walk("safe")
        .expect("actual production walker admits owned non-granting directories");
    assert_eq!(held.len(), 2, "fixture root and leaf retained");
    assert!(
        same_inspection(
            &stat(held[1].as_raw_fd()).expect("held metadata"),
            &stat(safe.as_raw_fd()).expect("created metadata")
        ),
        "exact held identity"
    );
    // SAFETY: safe is an owned fixture descriptor; no other object's permissions change.
    let result = unsafe { libc::fchmod(safe.as_raw_fd(), 0o720) };
    assert_eq!(result, 0, "unsafe-mode fixture");
    assert_eq!(scratch.walk("safe").err(), Some(Untrusted::Unsafe));
    // SAFETY: restore only this same held fixture object's mode for subsequent cases.
    let result = unsafe { libc::fchmod(safe.as_raw_fd(), 0o700) };
    assert_eq!(result, 0, "restore fixture mode");

    let outside = scratch.directory(c"outside");
    // SAFETY: exclusively create a fixed sentinel under the held owned outside fixture.
    let fd = unsafe {
        libc::openat(
            outside.as_raw_fd(),
            c"sentinel".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    assert!(fd >= 0, "exclusive sentinel creation");
    // SAFETY: successful open transferred this fresh fd exactly once.
    let mut sentinel = unsafe { File::from_raw_fd(fd) };
    scratch
        .record(&outside, c"sentinel")
        .expect("record sentinel identity");
    sentinel
        .write_all(b"outside-sentinel")
        .expect("write fixture sentinel");
    // SAFETY: the link and its relative target are fixed components in this owned scratch.
    let result = unsafe {
        libc::symlinkat(
            c"outside".as_ptr(),
            scratch.root.as_raw_fd(),
            c"link".as_ptr(),
        )
    };
    assert_eq!(result, 0, "exclusive symlink fixture");
    let root = scratch.root.try_clone().expect("retain fixture root");
    scratch
        .record(&root, c"link")
        .expect("record link identity");
    let error = scratch
        .walk("link")
        .expect_err("symlink must not be admitted");
    assert!(
        matches!(error, Untrusted::Reparse | Untrusted::Unsafe),
        "Darwin no-follow refusal"
    );
    sentinel.seek(SeekFrom::Start(0)).expect("rewind sentinel");
    let mut bytes = Vec::new();
    sentinel
        .read_to_end(&mut bytes)
        .expect("read owned sentinel");
    assert_eq!(bytes, b"outside-sentinel", "outside object remains intact");

    drop(held);
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_inspection_query_and_present_acl_refusal() {
    let mut scratch = Scratch::new();
    let safe = scratch.directory(c"safe");
    let expected = stat(safe.as_raw_fd()).expect("fixture metadata");
    let security = Filesec::new().expect("fresh filesec");
    assert_eq!(
        security.inspect(safe.as_raw_fd(), &expected),
        Ok(()),
        "completed descriptor security inspection"
    );
    drop(security);
    let security = Filesec::new().expect("fresh filesec");
    assert_eq!(
        security.inspect(-1, &expected),
        Err(Untrusted::Io),
        "real fstatx failure"
    );
    drop(security);
    let security = Filesec::new().expect("fresh filesec");
    let mut output = 0;
    // SAFETY: filesec is live; unknown property is an intentional native error fixture.
    let result = unsafe { filesec_query_property(security.0.as_ptr(), -1, &raw mut output) };
    assert_eq!(result, -1, "unknown property must fail");
    assert_eq!(
        acl_absent(result, output),
        Err(Untrusted::Io),
        "error is not zero/absence"
    );
    drop(security);

    for text in [
        c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n",
        c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny,inherited:write\n",
    ] {
        install_acl(&safe, text).expect("install fixed UUID deny-only fixture ACL");
        assert_eq!(
            scratch
                .walk("safe")
                .expect("supported deny-only ACL admitted")
                .len(),
            2,
            "fixture root and deny-only leaf retained"
        );
        let mut mismatch = stat(safe.as_raw_fd()).expect("ACL fixture metadata");
        mismatch.st_ino += 1;
        let security = Filesec::new().expect("fresh filesec");
        assert_eq!(
            security.inspect(safe.as_raw_fd(), &mismatch),
            Err(Untrusted::Io),
            "inspection mismatch precedes present-ACL refusal"
        );
    }
    for text in [
        c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::allow:read\n",
        c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\ngroup:11111111-2222-3333-4444-555555555555:::allow:read\n",
        c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::allow:read\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n",
    ] {
        install_acl(&safe, text).expect("install fixed UUID granting or mixed fixture ACL");
        assert_eq!(scratch.walk("safe").err(), Some(Untrusted::Unsafe), "every ALLOW refuses regardless of order");
    }
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}
