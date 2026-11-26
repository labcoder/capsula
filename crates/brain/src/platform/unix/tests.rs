use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::{ErrorKind, product_folder};

#[test]
fn product_folder_keeps_safe_existing_folders_and_refuses_shared_writes() {
    let scratch = crate::scratch::dir("product-folder");
    let dir = scratch.join("root").join("product");
    product_folder(&dir).expect("create product folder and its missing parent");
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let marker = dir.join("marker");
    fs::write(&marker, b"keep").unwrap();
    for (mode, expected) in [
        (0o755, Ok(())),
        (0o775, Err(ErrorKind::PermissionDenied)),
        (0o757, Err(ErrorKind::PermissionDenied)),
    ] {
        fs::set_permissions(&dir, Permissions::from_mode(mode)).unwrap();
        assert_eq!(product_folder(&dir).map_err(|error| error.kind()), expected);
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            mode
        );
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
    }
}

#[test]
fn product_folder_never_follows_the_leaf_link_or_accepts_a_file() {
    let scratch = crate::scratch::dir("product-folder-type");
    let dir = scratch.join("product");
    product_folder(&dir).unwrap();
    let link = scratch.join("alias");
    symlink(&dir, &link).unwrap();
    let file = scratch.join("not-a-folder");
    fs::write(&file, b"keep").unwrap();
    for path in [&link, &link.join(""), &file] {
        assert_eq!(
            product_folder(path).map_err(|error| error.kind()),
            Err(ErrorKind::PermissionDenied)
        );
    }
    assert_eq!(fs::read_link(&link).unwrap(), dir);
    assert_eq!(fs::read(&file).unwrap(), b"keep");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn product_folder_trusts_only_the_effective_user_or_root() {
    assert!(super::trusted_mode(0, 1000, 0o755));
    assert!(super::trusted_mode(1000, 1000, 0o700));
    assert!(!super::trusted_mode(1001, 1000, 0o700));
}

#[cfg(target_os = "linux")]
#[test]
fn product_folder_refuses_a_default_acl_even_with_mode_0700() {
    let scratch = crate::scratch::dir("product-folder-default-acl");
    let dir = scratch.join("product");
    product_folder(&dir).unwrap();
    let mut acl = 2_u32.to_le_bytes().to_vec();
    for (tag, permissions) in [(1_u16, 7_u16), (4, 7), (32, 0)] {
        acl.extend(tag.to_le_bytes());
        acl.extend(permissions.to_le_bytes());
        acl.extend(u32::MAX.to_le_bytes());
    }
    rustix::fs::setxattr(
        &dir,
        c"system.posix_acl_default",
        &acl,
        rustix::fs::XattrFlags::CREATE,
    )
    .expect("set the scratch folder's default ACL");
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        product_folder(&dir).map_err(|error| error.kind()),
        Err(ErrorKind::PermissionDenied)
    );
}
