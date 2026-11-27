use super::*;
use std::cell::RefCell;
use std::fs;
use std::io::{Read as _, Seek as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

thread_local! {
    pub(super) static ANCHOR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn set_anchor(anchor: Option<PathBuf>) {
    ANCHOR.with(|held| *held.borrow_mut() = anchor);
}

/// A fresh anchor folder under the commissioned fixture parent, removed through the adapter once it
/// still is the folder this made.
struct Anchor(PathBuf, (u64, u64));

fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

impl Anchor {
    fn new() -> Self {
        let parent = PathBuf::from(
            std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
                .expect("an explicitly commissioned fixture parent"),
        );
        let held = kumochi_storage::hold_chain(&parent).expect("admit the fixture parent's chain");
        let held = held.last().expect("the held fixture parent");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos();
        let path = parent.join(format!("brain-models-{}-{nanos}", std::process::id()));
        assert_eq!(
            identity(&fs::symlink_metadata(&parent).expect("the fixture parent")),
            identity(&held.metadata().expect("the held parent")),
            "the parent's name still gives the admitted folder"
        );
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("an exclusive anchor; never adopt a collision");
        let made = identity(&fs::symlink_metadata(&path).expect("the new anchor"));
        set_anchor(Some(path.clone()));
        Self(path, made)
    }

    fn part(&self) -> PathBuf {
        self.0.join("models/part")
    }

    fn finish(self) {
        assert_eq!(
            identity(&fs::symlink_metadata(&self.0).expect("the anchor")),
            self.1,
            "still the anchor this made"
        );
        assert_eq!(remove_model_tree(&self.0.join("models")), Ok(()));
        fs::remove_dir(&self.0).expect("the emptied anchor");
        set_anchor(None);
    }
}

/// A final published through the forwards, as the fetch does it.
fn install(part: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let held = create_model_dir(part).expect("private model folders");
    let target = part.join(name);
    let mut file =
        create_model_write(&part.join(format!("{name}.part"))).expect("an exclusive partial");
    file.write_all(bytes).expect("partial bytes");
    file.sync_all().expect("partial flush");
    publish_model(file, held.last().expect("part folder"), &target).expect("published sealed");
    target
}

fn digest(mut file: &File) -> Vec<u8> {
    file.rewind().expect("rewind");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("read the held file");
    Sha256::digest(&bytes).to_vec()
}

fn expected(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

#[expect(
    clippy::disallowed_methods,
    reason = "this test runs chmod as an owned child under the shared spawn lock"
)]
fn chmod(arguments: &[&str], path: &Path) {
    let _spawning = crate::platform::spawn_lock();
    let status = Command::new("/bin/chmod")
        .args(arguments)
        .arg(path)
        .env_clear()
        .status()
        .expect("chmod runs");
    assert!(status.success(), "chmod {arguments:?}");
}

/// A DENY entry on `folder`, cleared again on drop, so a failing test leaves a removable fixture.
struct Acl(PathBuf);

impl Acl {
    fn deny(folder: &Path) -> Self {
        chmod(&["+a", "everyone deny delete"], folder);
        Self(folder.to_owned())
    }
}

impl Drop for Acl {
    #[expect(
        clippy::disallowed_methods,
        reason = "this test runs chmod as an owned child under the shared spawn lock"
    )]
    fn drop(&mut self) {
        let _spawning = crate::platform::spawn_lock();
        let _cleared = Command::new("/bin/chmod")
            .arg("-N")
            .arg(&self.0)
            .env_clear()
            .status();
    }
}

#[test]
fn paths_refuse_outside_the_anchor_before_opening_anything() {
    let anchor = PathBuf::from("/kumochi-never-exists/Support");
    set_anchor(Some(anchor.clone()));
    assert_eq!(validate_model_path(&anchor.join("models/part")), Ok(()));
    for path in [
        "/kumochi-never-exists/models",
        "/kumochi-never-exists/Support/../models",
        "/kumochi-never-exists/Support",
    ] {
        assert_eq!(
            validate_model_path(Path::new(path)),
            Err(Untrusted::Unsafe),
            "{path}"
        );
        assert_eq!(
            hold_model_chain(Path::new(path)).err(),
            Some(Untrusted::Unsafe),
            "refused before admitting the anchor, which would be Missing: {path}"
        );
    }
    assert_eq!(
        model_file(&anchor.join("model.onnx")).err(),
        Some(Untrusted::Unsafe),
        "a file directly in the anchor"
    );
    assert_eq!(
        model_file(&anchor.join("models/part/model.onnx")).err(),
        Some(Untrusted::Missing),
        "a final's path passes, then the absent anchor"
    );
    set_anchor(None);
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_the_read_side_hashes_the_bytes_its_admitted_chain_holds() {
    let anchor = Anchor::new();
    let part = anchor.part();
    let target = install(&part, "model.onnx", b"weights");
    let chain = hold_model_chain(&part).expect("the admitted chain");
    let held = model_file(&target).expect("the sealed final");
    assert_eq!(fs::metadata(&target).expect("final").mode() & 0o7777, 0o400);
    assert_eq!(digest(&held), expected(b"weights"));
    assert_eq!(keep_model_final(&held, &target), Ok(()));

    let other = install(&part, "other.onnx", b"replacement");
    let spare = part.join("spare.onnx");
    fs::rename(&target, &spare).expect("the held final moves to a spare name");
    fs::rename(&other, &target).expect("a same-user swap of the final's name");
    for name in [&spare, &target] {
        assert_eq!(
            fs::symlink_metadata(name).expect("one name each").nlink(),
            1
        );
    }
    assert_eq!(
        digest(&held),
        expected(b"weights"),
        "the held object, not the name"
    );
    assert_eq!(check_model_final(&held, &target), Err(Untrusted::Unsafe));
    assert_eq!(keep_model_final(&held, &target), Err(Untrusted::Unsafe));
    let swapped = model_file(&target).expect("another valid sealed final");
    assert_eq!(
        digest(&swapped),
        expected(b"replacement"),
        "the documented limit: a swap to another valid final is admitted on its own"
    );
    drop((held, swapped));

    fs::remove_file(&spare).expect("drop the spare");
    fs::remove_file(&target).expect("drop the final");
    fs::write(&target, b"unsealed").expect("a writable final");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("0600");
    assert_eq!(model_file(&target).err(), Some(Untrusted::Unsafe));
    fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).expect("0400");
    let second = part.join("second.onnx");
    fs::hard_link(&target, &second).expect("a second name");
    assert_eq!(
        model_file(&target).err(),
        Some(Untrusted::Unsafe),
        "a hard-linked final refuses on the Mac"
    );
    fs::remove_file(&second).expect("drop the second name");
    let link = part.join("link.onnx");
    std::os::unix::fs::symlink(&target, &link).expect("a link to the final");
    assert_eq!(model_file(&link).err(), Some(Untrusted::Reparse));
    fs::remove_file(&link).expect("drop the link");

    drop(chain);
    anchor.finish();
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_an_acl_on_a_model_folder_refuses_the_chain_and_its_files() {
    let anchor = Anchor::new();
    let part = anchor.part();
    let target = install(&part, "model.onnx", b"weights");
    for folder in [part.clone(), anchor.0.join("models")] {
        let acl = Acl::deny(&folder);
        assert_eq!(
            hold_model_chain(&part).err(),
            Some(Untrusted::Unsafe),
            "{folder:?}"
        );
        assert_eq!(
            model_file(&target).err(),
            Some(Untrusted::Unsafe),
            "{folder:?}"
        );
        assert_eq!(
            remove_model_tree(&part),
            Err(Untrusted::Unsafe),
            "removal refuses without deleting: {folder:?}"
        );
        assert_eq!(fs::read(&target).expect("kept"), b"weights");
        drop(acl);
    }
    assert_eq!(
        model_file(&target).map(|file| digest(&file)),
        Ok(expected(b"weights"))
    );
    anchor.finish();
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_a_bad_final_and_a_bad_partial_are_discarded_by_name_and_identity() {
    let anchor = Anchor::new();
    let part = anchor.part();
    let target = install(&part, "model.onnx", b"stale");
    let held = create_model_dir(&part).expect("folders");
    let parent = held.last().expect("part folder");
    let final_file = model_final(&target).expect("the sealed final");
    assert_eq!(discard_model(final_file, parent, &target), Ok(()));
    assert!(!target.exists());

    let partial = part.join("model.onnx.part");
    let file = create_model_write(&partial).expect("an exclusive partial");
    assert_eq!(
        open_model_write(&partial).err(),
        Some(Untrusted::Io),
        "the held partial's lock"
    );
    let moved = part.join("moved.part");
    fs::rename(&partial, &moved).expect("move the held partial away");
    fs::write(&partial, b"arrival").expect("an arrival at the partial's name");
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o600)).expect("a private arrival");
    assert_eq!(
        discard_model(file, parent, &partial),
        Err(Untrusted::Unsafe)
    );
    assert_eq!(fs::read(&partial).expect("kept"), b"arrival");
    let resumed = resume_model_write(
        open_model_write(&moved).expect("the moved partial opens, unlocked"),
        &moved,
    )
    .expect("a writable partial is kept");
    let creation = hold_model_creation(&part).expect("the part folder, held private");
    assert_eq!(discard_model(resumed, &creation, &moved), Ok(()));
    assert!(!moved.exists());

    drop(held);
    anchor.finish();
}
