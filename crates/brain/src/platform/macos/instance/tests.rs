use super::*;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::PathBuf;

use crate::startup::custody::SessionCustody;

#[test]
fn a_view_releases_its_wiped_backing_when_its_last_reference_drops() {
    let key = [0xA5_u8; CAPABILITY_LEN];
    let before = RELEASED.get();
    let view = owned_view(&key).expect("a view");
    assert_eq!(view.to_vec(), key.to_vec());
    let second = view.clone();
    drop(view);
    assert_eq!(RELEASED.get(), before, "a live reference keeps the backing");
    drop(second);
    assert_eq!(
        RELEASED.get(),
        before + 1,
        "the backing was released, and so wiped"
    );
}

#[test]
fn another_edition_refuses_before_any_folder_lookup() {
    for identity in [ProductIdentity::RELEASE, ProductIdentity::GATE] {
        let refused = custody_in(identity, || panic!("no folder lookup")).map(drop);
        assert_eq!(refused, Err(CustodyError::NotAdmitted), "{identity:?}");
    }
}

#[test]
#[ignore = "calls SecTask on this process: run only in the signed, provisioned Dev brain host"]
fn signed_host_admits_the_dev_brain_before_any_item_call() {
    assert!(KeychainItem::admit().is_ok());
}

/// A fresh private Support folder under the commissioned fixture parent.
fn support(label: &str) -> PathBuf {
    let parent = std::env::var_os("STORAGE_ADMISSION_FIXTURE_PARENT")
        .expect("an explicitly commissioned fixture parent");
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).expect("fixture name entropy");
    let support = Path::new(&parent).join(format!(
        "custody-{label}-{:032x}",
        u128::from_ne_bytes(random)
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&support)
        .expect("exclusive fixture anchor");
    support
}

/// Removes what a fixture made, by exact name, and nothing else.
fn clean(support: &Path, names: &[&str]) {
    let product = support.join(DATA_DIR);
    for name in names {
        std::fs::remove_file(product.join(name)).expect("remove a fixture entry");
    }
    if product.exists() {
        std::fs::remove_dir(&product).expect("remove the fixture product folder");
    }
    std::fs::remove_dir(support).expect("remove the fixture anchor");
}

fn entries(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .expect("list the fixture folder")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("fixture metadata")
        .permissions()
        .mode()
        & 0o7777
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_custody_makes_only_the_private_product_folder_and_is_busy_until_it_drops() {
    let support = support("planted-path-sentinel");
    let first = SessionCustody::bootstrap_at(&support);
    let product = support.join(DATA_DIR);
    let made = (mode(&product), entries(&product));
    let second = SessionCustody::bootstrap_at(&support).map(drop);
    let shown = first
        .as_ref()
        .map(|custody| format!("{custody:?}"))
        .map_err(|error| *error);
    drop(first);
    let after = SessionCustody::bootstrap_at(&support).map(drop);
    clean(&support, &[SESSION_LOCK]);
    assert_eq!(
        made,
        (0o700, vec![SESSION_LOCK.to_owned()]),
        "no logs folder"
    );
    assert_eq!(second, Err(CustodyError::Busy));
    assert_eq!(after, Ok(()), "dropping custody releases the lock");
    let shown = shown.expect("custody");
    assert_eq!(shown, "SessionCustody(..)");
    assert!(!shown.contains("sentinel"));
}

#[test]
#[ignore = "requires exact-head review and a bounded native fixture-parent commission"]
fn native_custody_refuses_an_unsafe_lock_leaf_or_product_folder_without_repair() {
    let leaf = |label: &str, plant: &dyn Fn(&Path)| {
        let support = support(label);
        let product = support.join(DATA_DIR);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&product)
            .expect("fixture product folder");
        plant(&product.join(SESSION_LOCK));
        let refused = SessionCustody::bootstrap_at(&support).map(drop);
        let kept = std::fs::symlink_metadata(product.join(SESSION_LOCK))
            .expect("the planted leaf")
            .file_type();
        clean(&support, &[SESSION_LOCK]);
        (refused, kept)
    };
    let (refused, kept) = leaf("link", &|path| {
        std::os::unix::fs::symlink("elsewhere", path).expect("fixture link");
    });
    assert_eq!(refused, Err(CustodyError::Untrusted), "a link");
    assert!(kept.is_symlink());
    let (refused, kept) = leaf("fifo", &|path| {
        #[expect(
            clippy::disallowed_methods,
            reason = "a fixture's one child, started before any handle exists to inherit"
        )]
        let made = std::process::Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(path)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "fixture FIFO");
    });
    assert_eq!(refused, Err(CustodyError::Untrusted), "a FIFO");
    assert!(!kept.is_file());
    let (refused, _) = leaf("group", &|path| {
        std::fs::write(path, b"").expect("fixture leaf");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .expect("fixture mode");
    });
    assert_eq!(
        refused,
        Err(CustodyError::Untrusted),
        "a group-writable leaf"
    );

    let support = support("open-product");
    let product = support.join(DATA_DIR);
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(&product)
        .expect("fixture product folder");
    std::fs::set_permissions(&product, std::fs::Permissions::from_mode(0o755))
        .expect("fixture mode");
    let refused = SessionCustody::bootstrap_at(&support).map(drop);
    let (kept, inside) = (mode(&product), entries(&product));
    clean(&support, &[]);
    assert_eq!(refused, Err(CustodyError::Untrusted));
    assert_eq!((kept, inside), (0o755, vec![]), "refused without repair");
}
