//! The profile probes' anchors and lookups: the Known Folders on Windows, Application Support from
//! OS discovery on macOS, never an environment variable or the Gate override, whose admission
//! creates folders. Every lookup creates nothing, and each held chain lives as long as its result.

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
    ProductFolder as Found, SetupGuard, Support, admit_support, application_support_dir,
};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_RoamingAppData};

use crate::profile::Folder;

/// This edition's legacy product folders, each chain held while this lives.
pub(crate) struct Legacy {
    /// The local data folder: store, authority and session lock.
    pub(crate) data: Folder,
    /// The roaming settings folder, which only Windows keeps apart.
    pub(crate) roaming: Option<Folder>,
    _held: Vec<Found>,
}

/// Looks up the product folder under `anchor`, keeping what the lookup holds in `held`.
fn look(anchor: Option<PathBuf>, held: &mut Vec<Found>) -> Folder {
    let Some(anchor) = anchor else {
        return Folder::Unknown;
    };
    #[cfg(windows)]
    let found = hold_product_folder(&anchor).ok();
    #[cfg(target_os = "macos")]
    let found = admit_support(&anchor)
        .ok()
        .and_then(|support| support.product_folder(DATA_DIR).ok());
    let folder = match &found {
        None => return Folder::Unknown,
        Some(Found::Missing(_)) => Folder::Missing,
        Some(Found::Held(_)) => Folder::Present(anchor.join(DATA_DIR)),
    };
    held.extend(found);
    folder
}

/// The setup guard, taken without waiting.
#[cfg(windows)]
pub(crate) fn setup_guard() -> Result<HeldGuard, GuardError> {
    acquire_setup_guard()
}

/// The new-format root slot under the local Known Folder.
#[cfg(windows)]
pub(crate) fn profile_root() -> Root {
    local().map_or(Root::Unverified, |anchor| {
        kumochi_storage::profile_root(&anchor)
    })
}

/// The legacy local data and roaming settings folders under their Known Folders.
#[cfg(windows)]
pub(crate) fn legacy() -> Legacy {
    let mut held = Vec::new();
    let data = look(local(), &mut held);
    let roaming = look(known_folder(FOLDERID_RoamingAppData).ok(), &mut held);
    Legacy {
        data,
        roaming: Some(roaming),
        _held: held,
    }
}

#[cfg(windows)]
fn local() -> Option<PathBuf> {
    known_folder(FOLDERID_LocalAppData).ok()
}

/// The setup guard with the admitted Support anchor it was taken on, both held.
#[cfg(target_os = "macos")]
pub(crate) struct HeldGuard {
    _guard: SetupGuard,
    _support: Support,
}

/// The setup guard, taken without waiting. An anchor that can't be found or admitted is
/// `Unavailable`.
#[cfg(target_os = "macos")]
pub(crate) fn setup_guard() -> Result<HeldGuard, GuardError> {
    let support = admitted().ok_or(GuardError::Unavailable)?;
    Ok(HeldGuard {
        _guard: support.try_setup_guard()?,
        _support: support,
    })
}

/// The new-format root slot under the admitted Support anchor.
#[cfg(target_os = "macos")]
pub(crate) fn profile_root() -> Root {
    admitted().map_or(Root::Unverified, Support::profile_root)
}

/// The legacy product folder under the Support anchor; macOS keeps no roaming folder.
#[cfg(target_os = "macos")]
pub(crate) fn legacy() -> Legacy {
    let mut held = Vec::new();
    let data = look(application_support_dir().ok(), &mut held);
    Legacy {
        data,
        roaming: None,
        _held: held,
    }
}

#[cfg(target_os = "macos")]
fn admitted() -> Option<Support> {
    admit_support(&application_support_dir().ok()?).ok()
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
            Folder::Present(path) => assert_eq!(path, anchor.join(DATA_DIR)),
            _ => panic!("expected the product folder present"),
        }
        assert_eq!(held.len(), 2);
    }
}
