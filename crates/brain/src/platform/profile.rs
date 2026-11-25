//! The profile probes' anchors and lookups: the Known Folders on Windows, Application Support from
//! OS discovery on macOS, never an environment variable or the Gate override, whose admission
//! creates folders. Every lookup creates nothing, and each held chain lives as long as its result.
//!
//! On macOS the setup guard is a lock on the admitted Support folder, so the guard keeps that
//! anchor and every lookup opens its child relative to it: a rename of the path can't make the
//! guard protect one folder while a lookup describes another. On Windows the guard is a named
//! mutex, not a folder, so each probe holds its own anchor chain and opens the child relative to
//! the held anchor.

use std::path::PathBuf;

use kumochi_protocol::constants::DATA_DIR;
use kumochi_protocol::profile::Root;
pub(crate) use kumochi_storage::GuardError;
#[cfg(windows)]
pub(crate) use kumochi_storage::SetupGuard as HeldGuard;
#[cfg(windows)]
use kumochi_storage::{Child as Found, acquire_setup_guard, hold_product_folder, known_folder};
#[cfg(target_os = "macos")]
use kumochi_storage::{
    HeldFolder as Found, SetupGuard, Support, admit_support, application_support_dir,
};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_RoamingAppData};

#[cfg(target_os = "macos")]
use super::FolderId;
#[cfg(windows)]
use super::windows::held_folder_id;
use crate::profile::Folder;

/// This edition's legacy product folders, each chain held while this lives.
pub(crate) struct Legacy {
    /// The local data folder: store, authority and session lock.
    pub(crate) data: Folder,
    /// The roaming settings folder, which only Windows keeps apart.
    pub(crate) roaming: Option<Folder>,
    _held: Vec<Found>,
}

/// The setup guard, taken without waiting.
#[cfg(windows)]
pub(crate) fn setup_guard() -> Result<HeldGuard, GuardError> {
    acquire_setup_guard()
}

/// The new-format root slot under the local Known Folder.
#[cfg(windows)]
pub(crate) fn profile_root(_guard: &HeldGuard) -> Root {
    local().map_or(Root::Unverified, |anchor| {
        kumochi_storage::profile_root(&anchor)
    })
}

/// The legacy local data and roaming settings folders under their Known Folders.
#[cfg(windows)]
pub(crate) fn legacy(_guard: &HeldGuard) -> Legacy {
    let mut held = Vec::new();
    let data = look(local(), &mut held);
    let roaming = look(known_folder(FOLDERID_RoamingAppData).ok(), &mut held);
    Legacy {
        data,
        roaming: Some(roaming),
        _held: held,
    }
}

/// Looks up the product folder under `anchor`, keeping what the lookup holds in `held`.
#[cfg(windows)]
fn look(anchor: Option<PathBuf>, held: &mut Vec<Found>) -> Folder {
    let Some(anchor) = anchor else {
        return Folder::Unknown;
    };
    let Ok(found) = hold_product_folder(&anchor) else {
        return Folder::Unknown;
    };
    let folder = match &found {
        Found::Missing(_) => Folder::Missing,
        Found::Held(chain) => match chain.last().and_then(held_folder_id) {
            Some(id) => Folder::Present(anchor.join(DATA_DIR), id),
            None => Folder::Unknown,
        },
    };
    held.push(found);
    folder
}

#[cfg(windows)]
fn local() -> Option<PathBuf> {
    known_folder(FOLDERID_LocalAppData).ok()
}

/// The setup guard with the admitted Support anchor it locks, both held, and that anchor's path.
#[cfg(target_os = "macos")]
pub(crate) struct HeldGuard {
    _guard: SetupGuard,
    support: Support,
    path: PathBuf,
}

/// The setup guard, taken without waiting. An anchor that can't be found or admitted is
/// `Unavailable`.
#[cfg(target_os = "macos")]
pub(crate) fn setup_guard() -> Result<HeldGuard, GuardError> {
    let path = application_support_dir().map_err(|_| GuardError::Unavailable)?;
    let support = admit_support(&path).map_err(|_| GuardError::Unavailable)?;
    Ok(HeldGuard {
        _guard: support.try_setup_guard()?,
        support,
        path,
    })
}

/// The new-format root slot in the guard's Support anchor.
#[cfg(target_os = "macos")]
pub(crate) fn profile_root(guard: &HeldGuard) -> Root {
    guard.support.profile_root()
}

/// The legacy product folder in the guard's Support anchor; macOS keeps no roaming folder.
#[cfg(target_os = "macos")]
pub(crate) fn legacy(guard: &HeldGuard) -> Legacy {
    use std::os::unix::fs::MetadataExt as _;
    let mut held = Vec::new();
    let data = match guard.support.child(DATA_DIR) {
        Ok(None) => Folder::Missing,
        Ok(Some(found)) => match found.metadata() {
            Ok(metadata) => {
                held.push(found);
                Folder::Present(
                    guard.path.join(DATA_DIR),
                    FolderId {
                        volume: metadata.dev(),
                        file: u128::from(metadata.ino()),
                    },
                )
            }
            Err(_) => Folder::Unknown,
        },
        Err(_) => Folder::Unknown,
    };
    Legacy {
        data,
        roaming: None,
        _held: held,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::Scratch;

    #[test]
    fn only_a_product_folder_not_found_under_a_held_anchor_is_missing() {
        let scratch = Scratch::trusted("profile-legacy-look");
        let anchor = scratch.dir().to_owned();
        drop(kumochi_storage::owner_only_dir(&anchor).expect("an owner-only anchor"));
        let mut held = Vec::new();
        assert!(
            matches!(look(None, &mut held), Folder::Unknown),
            "no anchor"
        );
        assert!(
            matches!(
                look(Some(anchor.join("absent")), &mut held),
                Folder::Unknown
            ),
            "a missing anchor"
        );
        assert!(held.is_empty());
        assert!(matches!(
            look(Some(anchor.clone()), &mut held),
            Folder::Missing
        ));
        assert!(!anchor.join(DATA_DIR).exists(), "looking created nothing");
        fs::create_dir(anchor.join(DATA_DIR)).expect("the product folder");
        match look(Some(anchor.clone()), &mut held) {
            Folder::Present(path, id) => {
                assert_eq!(path, anchor.join(DATA_DIR));
                assert_eq!(Some(id), super::super::folder_id(&path));
            }
            _ => panic!("expected the product folder present"),
        }
        assert_eq!(held.len(), 2);
    }
}
