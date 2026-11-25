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

#[test]
fn regular_privacy_requires_one_link_and_exact_private_permissions() {
    let mut file = sample();
    file.st_mode = libc::S_IFREG | 0o600;
    file.st_nlink = 1;
    assert_eq!(safe_regular(&file, 501), Ok(()));
    for links in [0, 2, 3] {
        file.st_nlink = links;
        assert_eq!(safe_regular(&file, 501), Err(Untrusted::Unsafe));
    }
    file.st_nlink = 1;
    for mode in [0o400, 0o640, 0o606, 0o1600, 0o4600] {
        file.st_mode = libc::S_IFREG | mode;
        assert_eq!(safe_regular(&file, 501), Err(Untrusted::Unsafe));
    }
    file.st_mode = libc::S_IFREG | 0o600;
    assert_eq!(safe_regular(&file, 0), Err(Untrusted::Unsafe));
    file.st_mode = libc::S_IFDIR | 0o600;
    assert_eq!(safe_regular(&file, 501), Err(Untrusted::Unsafe));
}

#[test]
fn returned_acl_types_are_bound_to_directory_or_regular_metadata() {
    let mut file = sample();
    file.st_mode = libc::S_IFREG | 0o600;
    file.st_nlink = 1;
    let mut bytes = acl_response(&[(2, 4)]);
    put_word(&mut bytes, 28, 1);
    put_word(&mut bytes, 40, 0o600);
    assert_eq!(classify_present_acl(0, &bytes, &file), Ok(()));
    put_word(&mut bytes, 28, 2);
    assert_eq!(classify_present_acl(0, &bytes, &file), Err(Untrusted::Io));
    let mut directory = acl_response(&[]);
    put_word(&mut directory, 28, 1);
    assert_eq!(
        classify_present_acl(0, &directory, &sample()),
        Err(Untrusted::Io)
    );
    put_word(&mut bytes, 28, 1);
    put_word(&mut bytes, 120, 1);
    assert_eq!(
        classify_present_acl(0, &bytes, &file),
        Err(Untrusted::Unsafe)
    );
    assert_eq!(classify_present_acl(-1, &bytes, &file), Err(Untrusted::Io));
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
    for start in [56_usize, 61] {
        let mut bytes = acl_response(&[]);
        let payload = bytes[60..].to_vec();
        bytes.resize(start + 44, 0);
        bytes[start..].copy_from_slice(&payload);
        bytes[52..60].copy_from_slice(&91_u64.to_ne_bytes());
        put_word(
            &mut bytes,
            0,
            u32::try_from(start + 44).expect("small fixture"),
        );
        put_word(
            &mut bytes,
            44,
            u32::try_from(start - 44).expect("small fixture"),
        );
        assert_eq!(
            classify_present_acl(0, &bytes, &sample()),
            Err(Untrusted::Io),
            "payload start {start}"
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
#[expect(
    clippy::too_many_lines,
    reason = "one fixture exercises lock, log and retention on the same admitted namespace"
)]
fn native_shell_files_append_reopen_and_skip_unsafe_candidates() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut scratch = Scratch::new();
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    let support =
        std::path::PathBuf::from(parent).join(OsStr::from_bytes(scratch.residual.to_bytes()));
    let storage = crate::open_shell_storage(&support, "product").expect("admit fixture storage");
    let root = scratch.root.try_clone().expect("retain fixture parent");
    scratch.record(&root, c"product").expect("record product");
    let product = open_directory(root.as_raw_fd(), c"product").expect("fixture product");
    scratch.record(&product, c"logs").expect("record logs");
    let logs = open_directory(product.as_raw_fd(), c"logs").expect("fixture logs");
    let first = storage
        .open_instance_lock("shell-7.lock")
        .expect("private lock");
    scratch
        .record(&product, c"shell-7.lock")
        .expect("record lock");
    first.try_lock().expect("uncontended private fixture lock");
    std::fs::write(support.join("product/shell-7.lock"), b"lock-sentinel")
        .expect("seed only the recorded fixture lock");
    let log = std::sync::Arc::new(
        storage
            .append_log("shell.2026-11-09.log")
            .expect("private log"),
    );
    scratch
        .record(&logs, c"shell.2026-11-09.log")
        .expect("record log");
    let second_storage =
        crate::open_shell_storage(&support, "product").expect("reopen admitted storage");
    let second = second_storage
        .open_instance_lock("shell-7.lock")
        .expect("reopen lock without truncation");
    assert!(matches!(
        second.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(second);
    assert_eq!(
        std::fs::read(support.join("product/shell-7.lock")).expect("fixture lock bytes"),
        b"lock-sentinel"
    );
    drop(storage);
    let mut shared = log.as_ref();
    shared
        .write_all(b"metadata-only\n")
        .expect("shared append while other opaque and fixture owners remain");
    shared.flush().expect("synchronous flush");
    let again = second_storage
        .append_log("shell.2026-11-09.log")
        .expect("existing private append");
    (&again)
        .write_all(b"second\n")
        .expect("append without truncating");
    assert_eq!(
        std::fs::read(support.join("product/logs/shell.2026-11-09.log")).expect("fixture bytes"),
        b"metadata-only\nsecond\n"
    );
    let old = second_storage
        .append_log("shell.2026-11-08.log")
        .expect("old private log");
    scratch
        .record(&logs, c"shell.2026-11-08.log")
        .expect("record old log");
    drop(old);
    let unsafe_old = second_storage
        .append_log("shell.2026-11-07.log")
        .expect("private candidate");
    scratch
        .record(&logs, c"shell.2026-11-07.log")
        .expect("record candidate");
    drop(unsafe_old);
    std::fs::set_permissions(
        support.join("product/logs/shell.2026-11-07.log"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("change only fixture candidate permissions");
    assert_eq!(
        second_storage.retain_logs("shell", 1, &again),
        Ok(crate::Retention {
            scanned: 5,
            skipped: 4,
            removed: 1,
            incomplete: false
        })
    );
    assert_eq!(
        std::fs::symlink_metadata(support.join("product/logs/shell.2026-11-08.log"))
            .expect_err("old candidate removed")
            .kind(),
        std::io::ErrorKind::NotFound
    );
    scratch
        .entries
        .retain(|entry| entry.name.as_c_str() != c"shell.2026-11-08.log");
    assert_eq!(
        std::fs::metadata(support.join("product/logs/shell.2026-11-07.log"))
            .expect("unsafe candidate left alone")
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    drop(again);
    drop(first);
    let third = second_storage
        .open_instance_lock("shell-7.lock")
        .expect("lock remains named");
    third
        .try_lock()
        .expect("dropping the prior owner releases its lock");
    assert_eq!(
        std::fs::read(support.join("product/shell-7.lock")).expect("fixture lock bytes"),
        b"lock-sentinel"
    );
    drop(third);
    drop(log);
    drop(second_storage);
    drop(logs);
    drop(product);
    drop(root);
    scratch.cleanup().expect("remove recorded fixture objects");
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

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_returned_deny_acl_policy() {
    let mut scratch = Scratch::new();
    let safe = scratch.directory(c"safe");
    for (text, expected_flags) in [
        (
            c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n",
            0x2,
        ),
        (
            c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny,inherited:write\n",
            0x12,
        ),
        (
            c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny,file_inherit,directory_inherit:write\n",
            0x62,
        ),
    ] {
        install_acl(&safe, text).expect("install fixed UUID deny fixture");
        let expected = stat(safe.as_raw_fd()).expect("owned fixture metadata");
        let security = Filesec::new().expect("fresh filesec");
        // SAFETY: all-zero libc stat is valid writable storage, not inspection evidence.
        let mut observed: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the fixture fd is borrowed, metadata is writable, and filesec remains live.
        let result = unsafe {
            fstatx_np(safe.as_raw_fd(), &raw mut observed, security.0.as_ptr())
        };
        assert_eq!(result, 0, "completed fixture filesec acquisition");
        assert!(same_inspection(&expected, &observed), "fixture binding");
        let mut present = -1;
        // SAFETY: the populated filesec and int presence output remain live.
        let presence = unsafe {
            filesec_query_property(security.0.as_ptr(), FILESEC_ACL, &raw mut present)
        };
        assert_eq!(presence, 0, "completed presence query");
        assert!(present > 0, "deny fixture must remain explicitly present");

        let (result, buffer) = query_present_acl(safe.as_raw_fd());
        assert_eq!(result, 0, "actual bounded fd query");
        let word = |offset| {
            u32::from_ne_bytes(attribute_bytes(&buffer.0, offset).expect("bounded numeric field"))
        };
        let returned = word(4);
        assert_eq!(returned, 0x8243_800a, "complete returned common bitmap");
        for offset in [8, 12, 16, 20] {
            assert_eq!(word(offset), 0, "no other returned groups");
        }
        let length = word(0);
        assert!((128..=4096).contains(&length), "bounded one-entry response");
        let reference = i32::from_ne_bytes(
            attribute_bytes(&buffer.0, 44).expect("bounded signed reference"),
        );
        assert!(reference >= 16, "payload follows the fixed fields");
        let start = 44_usize
            .checked_add(usize::try_from(reference).expect("positive fixture reference"))
            .expect("bounded fixture start");
        assert!(start.is_multiple_of(4), "four-aligned payload");
        let payload_length = word(48);
        assert_eq!(payload_length, 68, "exact one-entry payload");
        assert_eq!(
            start.checked_add(68),
            Some(usize::try_from(length).expect("bounded fixture length")),
            "reference ends at the reported full length"
        );
        let count = word(start + 36);
        let acl_flags = word(start + 40);
        let flags = word(start + 60);
        let rights = word(start + 64);
        assert_eq!(count, 1, "one returned ACE, not normalization to empty");
        assert_eq!(acl_flags & !0x0002_0000, 0, "supported full ACL flags");
        assert_eq!(flags, expected_flags, "full raw DENY and inheritance flags");
        assert_eq!(rights, 0x4, "full raw write-data rights");
        assert_eq!(classify_present_acl(result, &buffer.0, &expected), Ok(()));
        eprintln!(
            "owned-deny presence_result={presence} presence={present} query={result} returned={returned:#x} length={length} reference={reference} payload_length={payload_length} count={count} acl_flags={acl_flags:#x} flags={flags:#x} rights={rights:#x}"
        );
    }
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_present_acl_query_failure() {
    let scratch = Scratch::new();
    let expected = stat(scratch.root.as_raw_fd()).expect("owned fixture metadata");
    let (result, buffer) = query_present_acl(-1);
    assert_eq!(
        result, -1,
        "actual bounded fd query fails, not earlier fstatx"
    );
    assert_eq!(
        classify_present_acl(result, &buffer.0, &expected),
        Err(Untrusted::Io)
    );
    eprintln!("owned-query-error result={result}");
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_product_lookup_creates_nothing_and_the_setup_guard_excludes_other_opens() {
    use crate::{AcquisitionError, GuardError, ProductFolder, admit_support};
    use std::os::unix::fs::PermissionsExt as _;

    let mut scratch = Scratch::new();
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    let support =
        std::path::PathBuf::from(parent).join(OsStr::from_bytes(scratch.residual.to_bytes()));
    let root = scratch.root.try_clone().expect("retain fixture parent");

    let anchor = admit_support(&support).expect("admit the fixture anchor");
    let guard = anchor.try_setup_guard().expect("an uncontended guard");
    assert_eq!(
        anchor.try_setup_guard().err(),
        Some(GuardError::Busy),
        "another open of the same anchor"
    );
    let other = admit_support(&support).expect("an independent walk to the same anchor");
    assert_eq!(
        other.try_setup_guard().err(),
        Some(GuardError::Busy),
        "an independently admitted anchor"
    );
    drop(guard);
    drop(
        other
            .try_setup_guard()
            .expect("dropping the holder released it"),
    );

    let anchor = match anchor.product_folder("product") {
        Ok(ProductFolder::Missing(anchor)) => anchor,
        other => panic!("expected the product folder missing, not {other:?}"),
    };
    assert_eq!(
        std::fs::symlink_metadata(support.join("product"))
            .expect_err("looking created nothing")
            .kind(),
        io::ErrorKind::NotFound
    );
    drop(
        anchor
            .try_setup_guard()
            .expect("the anchor is still held after a miss"),
    );
    assert_eq!(
        admit_support(&support.join("absent")).err(),
        Some(AcquisitionError::Open(io::ErrorKind::NotFound)),
        "a missing anchor is refused, not a missing product folder"
    );

    let product = scratch.directory(c"product");
    assert!(matches!(
        admit_support(&support)
            .expect("admit the fixture anchor")
            .product_folder("product"),
        Ok(ProductFolder::Held(_))
    ));
    std::fs::set_permissions(
        support.join("product"),
        std::fs::Permissions::from_mode(0o750),
    )
    .expect("loosen only the fixture product folder");
    assert_eq!(
        admit_support(&support)
            .expect("admit the fixture anchor")
            .product_folder("product")
            .err(),
        Some(AcquisitionError::Admission(Untrusted::Unsafe)),
        "a product folder others may read"
    );
    std::fs::set_permissions(
        support.join("product"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("restore the fixture product folder");

    std::fs::write(support.join("plain"), b"").expect("a fixture file at a product name");
    scratch.record(&root, c"plain").expect("record the file");
    std::os::unix::fs::symlink("product", support.join("linked")).expect("a fixture link");
    scratch.record(&root, c"linked").expect("record the link");
    // Darwin's O_DIRECTORY reports ENOTDIR for a link before O_NOFOLLOW's ELOOP.
    for name in ["plain", "linked"] {
        assert_eq!(
            admit_support(&support)
                .expect("admit the fixture anchor")
                .product_folder(name)
                .err(),
            Some(AcquisitionError::Admission(Untrusted::Unsafe)),
            "{name}"
        );
    }

    drop(product);
    drop(root);
    scratch.cleanup().expect("remove recorded fixture objects");
}

const GUARD_HOLDER: &str = "STORAGE_GUARD_HOLDER_ANCHOR";

/// Kills and reaps the holder process if the test ends early.
struct Holder(std::process::Child);

impl Drop for Holder {
    fn drop(&mut self) {
        if self.0.kill().is_ok() {
            self.0.wait().ok();
        }
    }
}

/// The child half of the cross-process guard test, spawned only by it with the anchor in
/// `GUARD_HOLDER`; without it there is nothing to hold. Holds the guard until killed.
#[test]
#[ignore = "spawned by native_setup_guard_is_busy_while_another_process_holds_it"]
fn native_setup_guard_holder() {
    let Some(support) = std::env::var_os(GUARD_HOLDER) else {
        return;
    };
    let anchor = crate::admit_support(Path::new(&support)).expect("admit the fixture anchor");
    let _guard = anchor
        .try_setup_guard()
        .expect("the holder takes the guard");
    // The test harness has already written this test's name without ending the line.
    let mut out = io::stdout();
    out.write_all(b"\nguard-held\n")
        .and_then(|()| out.flush())
        .expect("report holding");
    io::stdin()
        .read_to_end(&mut Vec::new())
        .expect("wait until the parent ends");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_setup_guard_is_busy_while_another_process_holds_it() {
    use crate::{GuardError, admit_support};
    use std::io::BufRead as _;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Command, Stdio};

    let scratch = Scratch::new();
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    let support =
        std::path::PathBuf::from(parent).join(OsStr::from_bytes(scratch.residual.to_bytes()));
    let anchor = admit_support(&support).expect("admit the fixture anchor");
    let mut holder = Holder(
        Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--exact",
                "platform::macos::protected::tests::native_setup_guard_holder",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(GUARD_HOLDER, &support)
            .env_remove("STORAGE_ADMISSION_FIXTURE_PARENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the holder"),
    );
    let output = holder.0.stdout.take().expect("the holder's piped output");
    assert!(
        io::BufReader::new(output)
            .lines()
            .map_while(Result::ok)
            .any(|line| line == "guard-held"),
        "the holder took the guard"
    );
    assert_eq!(
        anchor.try_setup_guard().err(),
        Some(GuardError::Busy),
        "another process holds it"
    );
    holder.0.kill().expect("kill the holder without releasing");
    let status = holder.0.wait().expect("reap the holder");
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "the holder was killed"
    );
    drop(
        anchor
            .try_setup_guard()
            .expect("the holder's exit released it"),
    );
    drop(holder);
    drop(anchor);
    scratch.cleanup().expect("remove recorded fixture objects");
}
