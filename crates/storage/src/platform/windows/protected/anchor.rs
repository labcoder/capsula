//! Looking up one of this edition's folders under a held, trusted anchor without creating
//! anything, so a missing folder is a definitive answer rather than any failure to open it.

use std::fs::File;
use std::io;
use std::path::Path;

use kumochi_protocol::constants::{DATA_DIR, ProductIdentity};
use kumochi_protocol::profile::Root;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY,
};

use super::{Aces, Kind, Security, Trusted, Untrusted, attributes, check, hold_chain, open_child};

/// A named folder under a trusted anchor.
#[derive(Debug)]
pub enum Child {
    /// The anchor's chain with the named folder last, every one held and trusted.
    Held(Vec<File>),
    /// The anchor's chain, held and trusted with no one untrusted able to add to it, and nothing
    /// at the name in it.
    Missing(Vec<File>),
}

/// Holds the chain of the absolute folder `anchor`, then looks up this edition's product folder
/// (`DATA_DIR`) in it. Missing means only that the open of that name under the held anchor folder
/// found no file; a missing or non-folder anchor, an anchor others may add to, a lost path, or
/// anything at the name that isn't a trusted plain folder is refused.
pub fn hold_product_folder(anchor: &Path) -> Result<Child, Untrusted> {
    hold_named(anchor, DATA_DIR)
}

/// Reads this edition's new-format root slot under `anchor`, creating nothing. Only a missing
/// slot is absent: before the allocator writes its record, a folder there is a footprint of
/// unknown origin, and any refusal or failure proves nothing either.
#[must_use]
pub fn profile_root(anchor: &Path) -> Root {
    match hold_named(anchor, ProductIdentity::ACTIVE.profile_root) {
        Ok(Child::Missing(_)) => Root::Absent,
        Ok(Child::Held(_)) | Err(_) => Root::Unverified,
    }
}

/// The lookup behind both. `name` is always one of this edition's fixed folder names, never input,
/// so Win32 name normalization and reserved device names can't make `Missing` describe another name.
fn hold_named(anchor: &Path, name: &str) -> Result<Child, Untrusted> {
    let mut chain = hold_chain(anchor)?;
    let trusted = Trusted::new()?;
    let held = chain.last().ok_or(Untrusted::Io)?;
    if attributes(held)? & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(Untrusted::Unsafe);
    }
    if Security::read(held)?.others_may(
        FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY,
        &trusted,
        Aces::Effective,
    )? {
        return Err(Untrusted::Unsafe);
    }
    let path = anchor.join(name);
    let child = match open_child(held, name) {
        Ok(child) => child,
        Err(error) if absent(&error) => return Ok(Child::Missing(chain)),
        Err(_) => return Err(Untrusted::Io),
    };
    check(&child, &path, Kind::Folder, &trusted)?;
    if attributes(&child)? & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(Untrusted::Unsafe);
    }
    chain.push(child);
    Ok(Child::Held(chain))
}

/// Only "file not found" from the folder's own open is absence; a lost path or drive isn't.
fn absent(error: &io::Error) -> bool {
    error.raw_os_error() == ERROR_FILE_NOT_FOUND.try_into().ok()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

    use super::*;
    use crate::scratch::Scratch;
    use crate::set_dacl;

    /// An owner-only anchor under the trusted local application data folder.
    fn anchor(name: &str) -> (Scratch, std::path::PathBuf) {
        let scratch = Scratch::under(
            &super::super::known_folder(FOLDERID_LocalAppData).expect("local app data"),
            name,
        );
        let anchor = scratch.join("anchor");
        drop(super::super::owner_only_dir(&anchor).expect("an owner-only anchor"));
        (scratch, anchor)
    }

    #[test]
    fn a_trusted_product_folder_is_held_after_its_anchor() {
        let (_scratch, anchor) = anchor("anchor-held");
        fs::create_dir(anchor.join(DATA_DIR)).expect("the product folder");
        let depth = hold_chain(&anchor).expect("the anchor chain").len();
        match hold_product_folder(&anchor) {
            Ok(Child::Held(chain)) => assert_eq!(chain.len(), depth + 1),
            other => panic!("expected the product folder held, not {other:?}"),
        }
    }

    #[test]
    fn only_a_product_folder_not_found_under_the_anchor_is_missing() {
        let (_scratch, anchor) = anchor("anchor-missing");
        let depth = hold_chain(&anchor).expect("the anchor chain").len();
        match hold_product_folder(&anchor) {
            Ok(Child::Missing(chain)) => assert_eq!(chain.len(), depth),
            other => panic!("expected the product folder missing, not {other:?}"),
        }
        assert!(!anchor.join(DATA_DIR).exists(), "looking created nothing");
        assert_eq!(
            hold_product_folder(&anchor.join("absent")).err(),
            Some(Untrusted::Missing),
            "a missing anchor is refused, not a missing product folder"
        );
    }

    #[test]
    fn a_trusted_file_as_the_anchor_is_refused() {
        let (_scratch, anchor) = anchor("anchor-file");
        let file = anchor.join("file");
        fs::write(&file, b"").expect("a file");
        assert_eq!(hold_product_folder(&file).err(), Some(Untrusted::Unsafe));
    }

    #[test]
    fn only_file_not_found_is_absence() {
        assert!(absent(&io::Error::from_raw_os_error(2)), "file not found");
        for code in [3, 15, 5, 123] {
            assert!(!absent(&io::Error::from_raw_os_error(code)), "{code}");
        }
    }

    #[test]
    fn a_file_at_the_name_or_an_anchor_others_may_add_to_is_refused() {
        let (_scratch, anchor) = anchor("anchor-refused");
        fs::write(anchor.join(DATA_DIR), b"").expect("a file at the name");
        assert_eq!(
            hold_product_folder(&anchor).err(),
            Some(Untrusted::Unsafe),
            "a file"
        );
        fs::remove_file(anchor.join(DATA_DIR)).expect("the file goes");
        set_dacl(&anchor, |user| {
            format!("D:P(A;OICI;FA;;;{user})(A;;0x4;;;WD)")
        });
        assert_eq!(
            hold_product_folder(&anchor).err(),
            Some(Untrusted::Unsafe),
            "others may add a folder"
        );
    }

    #[test]
    fn a_child_is_opened_in_the_held_folder_after_its_path_is_redirected() {
        let (_scratch, anchor) = anchor("anchor-relative");
        fs::create_dir(anchor.join("kept")).expect("a folder in the held anchor");
        let held = hold_chain(&anchor)
            .expect("the anchor chain")
            .pop()
            .expect("the anchor itself");
        let moved = anchor.with_file_name("moved");
        fs::rename(&anchor, &moved).expect("move the held anchor");
        fs::create_dir_all(anchor.join("planted")).expect("a replacement holding another child");
        let kept = open_child(&held, "kept").expect("the held folder's own child");
        assert_eq!(
            super::super::same_final_path(&kept, &moved.join("kept")),
            Ok(true)
        );
        assert_eq!(
            open_child(&held, "planted")
                .map(drop)
                .map_err(|error| error.raw_os_error()),
            Err(ERROR_FILE_NOT_FOUND.try_into().ok()),
            "the replacement's child isn't in the held folder"
        );
    }

    #[test]
    fn only_a_missing_name_is_absence() {
        use windows_sys::Win32::Foundation::{
            STATUS_ACCESS_DENIED, STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_NOT_FOUND,
            STATUS_OBJECTID_NOT_FOUND,
        };
        for (status, missing) in [
            (STATUS_OBJECT_NAME_NOT_FOUND, true),
            (STATUS_NO_SUCH_FILE, true),
            (STATUS_OBJECTID_NOT_FOUND, false),
            (STATUS_ACCESS_DENIED, false),
        ] {
            assert_eq!(
                absent(&super::super::status_error(status)),
                missing,
                "{status:#x}"
            );
        }
    }

    #[test]
    fn only_a_missing_root_slot_is_absent_and_reading_it_creates_nothing() {
        let (_scratch, anchor) = anchor("profile-root");
        let slot = anchor.join(ProductIdentity::ACTIVE.profile_root);
        assert_eq!(profile_root(&anchor), Root::Absent);
        assert!(!slot.exists(), "reading created nothing");
        assert_eq!(
            profile_root(&anchor.join("absent")),
            Root::Unverified,
            "a missing anchor"
        );
        fs::create_dir(&slot).expect("a folder at the slot");
        assert_eq!(profile_root(&anchor), Root::Unverified, "a folder");
        fs::remove_dir(&slot).expect("the folder goes");
        fs::write(&slot, b"sentinel").expect("a file at the slot");
        assert_eq!(profile_root(&anchor), Root::Unverified, "a file");
        assert_eq!(fs::read(&slot).expect("the file stays"), b"sentinel");
    }
}
