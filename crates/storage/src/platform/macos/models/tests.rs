use super::*;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use crate::platform::macos::protected::tests::{Scratch, install_acl};

const DENY: &CStr = c"!#acl 1\ngroup:11111111-2222-3333-4444-555555555555:::deny:write\n";

fn anchor(scratch: &Scratch) -> PathBuf {
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    PathBuf::from(parent).join(OsStr::from_bytes(scratch.residual.to_bytes()))
}

fn mode(path: &Path) -> (u32, u64) {
    use std::os::unix::fs::MetadataExt as _;
    let found = fs::symlink_metadata(path).expect("fixture entry");
    (found.mode() & 0o7777, found.nlink())
}

fn absent(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn read_all(mut file: &File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("read fixture file");
    bytes
}

/// A partial with `bytes` in it, made through the adapter.
fn partial(support: &Path, path: &Path, bytes: &[u8]) -> File {
    let mut file = create_private_file(support, path).expect("exclusive partial");
    file.write_all(bytes).expect("partial bytes");
    file.sync_all().expect("partial flush");
    file
}

#[test]
fn paths_refuse_outside_the_anchor_and_dots_before_opening_anything() {
    let support = Path::new("/kumochi-never-exists/Support");
    assert_eq!(
        validate_private_path(support, &support.join("product/models/part")),
        Ok(())
    );
    for path in [
        "/kumochi-never-exists/Support",
        "/kumochi-never-exists/Supporter/models",
        "/kumochi-never-exists/models",
        "/kumochi-never-exists/Support/./models",
        "/kumochi-never-exists/Support/../models",
        "//kumochi-never-exists/Support/models",
        "kumochi-never-exists/Support/models",
    ] {
        assert_eq!(
            validate_private_path(support, Path::new(path)),
            Err(Untrusted::Unsafe),
            "{path}"
        );
    }
    assert_eq!(
        validate_private_path(Path::new("Support"), Path::new("Support/models")),
        Err(Untrusted::Unsafe),
        "a relative anchor"
    );
    assert_eq!(
        hold_private_chain(support, Path::new("/kumochi-never-exists/models")).err(),
        Some(Untrusted::Unsafe),
        "lexical refusal comes before admitting the anchor, which would be Missing"
    );
    assert_eq!(
        create_private_file(support, &support.join("model.bin.part")).err(),
        Some(Untrusted::Unsafe),
        "a file directly in the anchor, outside every private folder"
    );
}

#[test]
fn partial_and_sealed_names_are_disjoint_ignoring_case_before_opening_anything() {
    let support = Path::new("/kumochi-never-exists/Support");
    let file = |name: &str| support.join("product/part").join(name);
    for name in ["model.bin", "model.part.bin", "part", ""] {
        assert_eq!(
            create_private_file(support, &file(name)).err(),
            Some(Untrusted::Unsafe),
            "create {name}"
        );
        assert_eq!(
            open_private_write(support, &file(name)).err(),
            Some(Untrusted::Unsafe),
            "resume {name}"
        );
    }
    for name in ["model.part", "model.PART", "model.Part", ".part"] {
        assert_eq!(
            open_sealed(support, &file(name), Seal::ReadOnly).err(),
            Some(Untrusted::Unsafe),
            "sealed {name}"
        );
    }
    assert_eq!(
        open_sealed(support, &file("model.bin"), Seal::ReadOnly).err(),
        Some(Untrusted::Missing),
        "a final's name passes, then the absent anchor"
    );
    assert_eq!(
        open_private_write(support, &file("model.bin.PART")).err(),
        Some(Untrusted::Missing),
        "a partial's name in any case passes, then the absent anchor"
    );
}

#[test]
fn seals_have_exact_modes() {
    assert_eq!(
        (Seal::ReadOnly.mode(), Seal::ReadExecute.mode()),
        (0o400, 0o500)
    );
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_a_partial_is_published_sealed_and_an_arrival_is_kept() {
    let scratch = Scratch::new();
    let support = anchor(&scratch);
    let part = support.join("product/models/part");
    let held = create_private_dir(&support, &part).expect("private folders");
    let parent = held.last().expect("part folder");
    assert_eq!(mode(&part).0, 0o700);
    assert_eq!(mode(&support.join("product/models")).0, 0o700);
    let source = part.join("model.bin.part");
    let target = part.join("model.bin");

    let file = partial(&support, &source, b"verified");
    assert_eq!(mode(&source), (0o600, 1));
    assert_eq!(
        create_private_file(&support, &source).err(),
        Some(Untrusted::Io),
        "an existing partial is never reopened by creation"
    );
    assert_eq!(
        open_private_write(&support, &source).err(),
        Some(Untrusted::Io),
        "the first handle's lock"
    );

    fs::write(&target, b"arrival").expect("an unexpected arrival");
    assert_eq!(
        seal_and_publish(&support, file, parent, &source, &target, Seal::ReadOnly),
        Err(Untrusted::Io)
    );
    assert_eq!(fs::read(&target).expect("arrival"), b"arrival");
    assert_eq!(
        mode(&source),
        (0o600, 1),
        "a failed publish leaves the partial resumable"
    );
    fs::remove_file(&target).expect("remove the arrival");

    let other = support.join("product/models/other");
    let elsewhere = create_private_dir(&support, &other).expect("another folder");
    let file = open_private_write(&support, &source).expect("resume the partial");
    assert_eq!(
        seal_and_publish(
            &support,
            file,
            elsewhere.last().expect("other folder"),
            &source,
            &target,
            Seal::ReadOnly
        ),
        Err(Untrusted::Reparse)
    );
    let file = open_private_write(&support, &source).expect("resume the partial");
    assert_eq!(
        seal_and_publish(
            &support,
            file,
            parent,
            &source,
            &other.join("model.bin"),
            Seal::ReadOnly
        ),
        Err(Untrusted::Unsafe),
        "names in different folders"
    );

    let file = open_private_write(&support, &source).expect("resume the partial");
    assert_eq!(
        seal_and_publish(
            &support,
            file,
            parent,
            &source,
            &part.join("final.part"),
            Seal::ReadOnly
        ),
        Err(Untrusted::Unsafe),
        "a published name never ends like a partial's"
    );
    assert_eq!(mode(&source), (0o600, 1));
    let file = open_private_write(&support, &source).expect("resume the partial");
    assert_eq!(
        seal_and_publish(
            &support,
            file,
            parent,
            &source,
            &part.join("final.PART"),
            Seal::ReadOnly
        ),
        Err(Untrusted::Unsafe),
        "in any case"
    );
    let file = open_private_write(&support, &source).expect("resume the partial");
    let unstaged = part.join("model.tmp");
    fs::rename(&source, &unstaged).expect("a partial at a name that isn't a partial's");
    assert_eq!(
        seal_and_publish(&support, file, parent, &unstaged, &target, Seal::ReadOnly),
        Err(Untrusted::Unsafe),
        "only a partial's name is published, so a stop after its seal is resumable"
    );
    assert_eq!(mode(&unstaged), (0o600, 1));
    assert!(absent(&target));
    fs::rename(&unstaged, &source).expect("back at the partial's name");

    let file = open_private_write(&support, &source).expect("resume the partial");
    assert_eq!(
        seal_and_publish(&support, file, parent, &source, &target, Seal::ReadOnly),
        Ok(())
    );
    assert!(absent(&source));
    assert_eq!(mode(&target), (0o400, 1), "published sealed");
    let sealed = open_sealed(&support, &target, Seal::ReadOnly).expect("the sealed final");
    assert_eq!(read_all(&sealed), b"verified");
    assert_eq!(
        keep_sealed(&support, &sealed, &target, Seal::ReadOnly),
        Ok(())
    );
    assert_eq!(check_named(&support, &sealed, &target), Ok(()));
    assert_eq!(
        open_private_write(&support, &target).err(),
        Some(Untrusted::Unsafe),
        "a sealed final is never unsealed"
    );
    assert_eq!(mode(&target), (0o400, 1));
    assert_eq!(
        open_sealed(&support, &target, Seal::ReadExecute).err(),
        Some(Untrusted::Unsafe),
        "the exact sealed mode"
    );

    drop((sealed, held, elsewhere));
    assert_eq!(
        remove_private_tree(&support, &support.join("product")),
        Ok(())
    );
    assert!(absent(&support.join("product")));
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_unsealed_linked_and_link_finals_refuse_before_opening() {
    let scratch = Scratch::new();
    let support = anchor(&scratch);
    let part = support.join("product/part");
    let held = create_private_dir(&support, &part).expect("private folders");
    let target = part.join("model.bin");

    fs::write(&target, b"bytes").expect("a final");
    fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("an unsealed final");
    assert_eq!(
        open_sealed(&support, &target, Seal::ReadOnly).err(),
        Some(Untrusted::Unsafe),
        "an unsealed 0600 final"
    );
    fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o400))
        .expect("seal the fixture");
    let linked = part.join("second");
    fs::hard_link(&target, &linked).expect("a second name");
    assert_eq!(
        open_sealed(&support, &target, Seal::ReadOnly).err(),
        Some(Untrusted::Unsafe),
        "a final with two names"
    );
    fs::remove_file(&linked).expect("drop the second name");
    assert_eq!(
        open_sealed(&support, &target, Seal::ReadOnly).map(|file| read_all(&file)),
        Ok(b"bytes".to_vec())
    );

    let link = part.join("link.bin");
    std::os::unix::fs::symlink(&target, &link).expect("a link to the final");
    assert_eq!(
        open_sealed(&support, &link, Seal::ReadOnly).err(),
        Some(Untrusted::Reparse)
    );
    let staged_link = part.join("link.part");
    std::os::unix::fs::symlink(&target, &staged_link).expect("a link at a partial's name");
    assert_eq!(
        open_private_write(&support, &staged_link).err(),
        Some(Untrusted::Reparse)
    );
    fs::remove_file(&staged_link).expect("drop the partial-named link");
    assert_eq!(
        open_sealed(&support, &part.join("absent"), Seal::ReadOnly).err(),
        Some(Untrusted::Missing)
    );
    fs::remove_file(&link).expect("drop the link");

    drop(held);
    assert_eq!(
        remove_private_tree(&support, &support.join("product")),
        Ok(())
    );
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_an_acl_or_a_shared_mode_on_a_model_folder_refuses() {
    let mut scratch = Scratch::new();
    let support = anchor(&scratch);
    let acl = scratch.directory(c"acl");
    install_acl(&acl, DENY).expect("install fixed UUID deny fixture");
    assert_eq!(
        hold_private_chain(&support, &support.join("acl")).err(),
        Some(Untrusted::Unsafe),
        "a DENY-only ACL still refuses a model folder"
    );
    assert_eq!(
        create_private_dir(&support, &support.join("acl/part")).err(),
        Some(Untrusted::Unsafe),
        "nothing is created under it"
    );
    assert!(absent(&support.join("acl/part")));

    let shared = scratch.directory(c"shared");
    // SAFETY: shared is the held fixture folder; the mode is a fixed fixture value.
    assert_eq!(unsafe { libc::fchmod(shared.as_raw_fd(), 0o755) }, 0);
    assert_eq!(
        hold_private_chain(&support, &support.join("shared")).err(),
        Some(Untrusted::Unsafe),
        "a root-like 0755 folder isn't private"
    );
    drop((acl, shared));
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_discard_deletes_only_the_object_it_holds() {
    let scratch = Scratch::new();
    let support = anchor(&scratch);
    let part = support.join("product/part");
    let held = create_private_dir(&support, &part).expect("private folders");
    let parent = held.last().expect("part folder");
    let name = part.join("model.bin.part");
    let moved = part.join("moved.part");

    let first = partial(&support, &name, b"first");
    fs::rename(&name, &moved).expect("move the held partial away");
    let second = partial(&support, &name, b"second");
    assert_eq!(
        discard_entry(&support, first, parent, &name),
        Err(Untrusted::Unsafe)
    );
    assert_eq!(fs::read(&name).expect("kept"), b"second");

    let other = create_private_dir(&support, &support.join("product/other")).expect("folder");
    let swapped = open_private_write(&support, &moved).expect("the moved partial reopens");
    assert_eq!(
        discard_entry(&support, swapped, other.last().expect("other"), &moved),
        Err(Untrusted::Reparse)
    );
    assert_eq!(fs::read(&moved).expect("kept"), b"first");

    assert_eq!(discard_entry(&support, second, parent, &name), Ok(()));
    assert!(absent(&name));

    drop((held, other));
    assert_eq!(
        remove_private_tree(&support, &support.join("product")),
        Ok(())
    );
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_removal_checks_the_whole_tree_before_deleting_anything() {
    let scratch = Scratch::new();
    let support = anchor(&scratch);
    let part = support.join("product/models/part");
    let held = create_private_dir(&support, &part).expect("private folders");
    let parent = held.last().expect("part folder");
    let source = part.join("model.bin.part");
    let target = part.join("model.bin");
    let file = partial(&support, &source, b"sealed");
    seal_and_publish(&support, file, parent, &source, &target, Seal::ReadOnly).expect("final");
    drop(partial(&support, &part.join("next.part"), b"partial"));
    let sub = create_private_dir(&support, &part.join("sub")).expect("a subfolder");
    drop(partial(&support, &part.join("sub/deep.part"), b"deep"));

    // SAFETY: the subfolder is held and both names are fixed and terminated.
    let made = unsafe {
        libc::symlinkat(
            c"elsewhere".as_ptr(),
            sub.last().expect("sub").as_raw_fd(),
            c"link".as_ptr(),
        )
    };
    assert_eq!(made, 0, "fixture link");
    assert_eq!(
        remove_private_tree(&support, &part),
        Err(Untrusted::Reparse)
    );
    for (kept, bytes) in [
        ("model.bin", &b"sealed"[..]),
        ("next.part", b"partial"),
        ("sub/deep.part", b"deep"),
    ] {
        assert_eq!(
            fs::read(part.join(kept)).expect("kept"),
            bytes,
            "{kept} survives a refused removal"
        );
    }
    fs::remove_file(part.join("sub/link")).expect("drop the link");

    let reader = open_sealed(&support, &target, Seal::ReadOnly).expect("a reader");
    assert_eq!(remove_private_tree(&support, &part), Ok(()));
    assert!(absent(&part));
    assert_eq!(
        read_all(&reader),
        b"sealed",
        "an open reader keeps its bytes"
    );
    assert_eq!(
        remove_private_tree(&support, &part),
        Ok(()),
        "an absent part counts as removed"
    );
    assert_eq!(
        remove_private_tree(&support, &support.join("product/absent/part")),
        Err(Untrusted::Missing),
        "a missing folder above the part isn't a removal"
    );

    drop((reader, held, sub));
    assert_eq!(
        remove_private_tree(&support, &support.join("product")),
        Ok(())
    );
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_removal_refuses_an_acl_child_and_a_foreign_mode_without_deleting() {
    let scratch = Scratch::new();
    let support = anchor(&scratch);
    let part = support.join("product/part");
    let held = create_private_dir(&support, &part).expect("private folders");
    drop(partial(&support, &part.join("kept.part"), b"kept"));
    let child = create_private_dir(&support, &part.join("child")).expect("child folder");
    install_acl(child.last().expect("child"), DENY).expect("install fixed UUID deny fixture");
    assert_eq!(remove_private_tree(&support, &part), Err(Untrusted::Unsafe));
    assert_eq!(fs::read(part.join("kept.part")).expect("kept"), b"kept");
    drop(child);
    fs::remove_dir(part.join("child")).expect("drop the ACL child");

    fs::set_permissions(
        part.join("kept.part"),
        std::os::unix::fs::PermissionsExt::from_mode(0o644),
    )
    .expect("a foreign mode");
    assert_eq!(remove_private_tree(&support, &part), Err(Untrusted::Unsafe));
    assert_eq!(fs::read(part.join("kept.part")).expect("kept"), b"kept");
    fs::set_permissions(
        part.join("kept.part"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("restore the mode");

    drop(held);
    assert_eq!(
        remove_private_tree(&support, &support.join("product")),
        Ok(())
    );
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_a_file_at_the_removed_folder_survives_and_keep_needs_the_seal() {
    let scratch = Scratch::new();
    let support = anchor(&scratch);
    let part = support.join("product/part");
    let held = create_private_dir(&support, &support.join("product")).expect("private folder");
    fs::write(&part, b"not a folder").expect("a file at the folder's name");
    fs::set_permissions(&part, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("a private file");
    assert_eq!(remove_private_tree(&support, &part), Err(Untrusted::Unsafe));
    assert_eq!(fs::read(&part).expect("kept"), b"not a folder");
    fs::remove_file(&part).expect("drop the fixture file");

    let held_part = create_private_dir(&support, &part).expect("part folder");
    let source = part.join("model.bin.part");
    let file = partial(&support, &source, b"bytes");
    assert_eq!(
        keep_sealed(&support, &file, &source, Seal::ReadOnly),
        Err(Untrusted::Unsafe),
        "a writable partial isn't a sealed final"
    );

    // A publish between its seal and its rename, still holding the lock, then one that stopped.
    fs::set_permissions(&source, std::os::unix::fs::PermissionsExt::from_mode(0o400))
        .expect("seal the held partial");
    assert_eq!(
        open_private_write(&support, &source).err(),
        Some(Untrusted::Io),
        "the publisher's lock"
    );
    assert_eq!(mode(&source), (0o400, 1), "a locked partial keeps its seal");
    drop(file);
    let resumed = open_private_write(&support, &source).expect("the stranded partial resumes");
    assert_eq!(mode(&source), (0o600, 1));
    assert_eq!(read_all(&resumed), b"bytes");
    let target = part.join("model.bin");
    assert_eq!(
        seal_and_publish(
            &support,
            resumed,
            held_part.last().expect("part"),
            &source,
            &target,
            Seal::ReadOnly
        ),
        Ok(())
    );
    assert_eq!(mode(&target), (0o400, 1));

    drop((held, held_part));
    assert_eq!(
        remove_private_tree(&support, &support.join("product")),
        Ok(())
    );
    scratch
        .cleanup()
        .expect("identity-checked cleanup; uncertainty leaves reported residual");
}
