//! Read-only probes behind the shared profile observer, and their composition over this machine's
//! own folders, setup guard and credential store.

use std::path::{Path, PathBuf};

use kumochi_protocol::constants::{
    AUTHORITY_FILE, PROFILE_OWNER_ACCOUNT, ProductIdentity, SESSION_LOCK, STORE_FILE,
    account_target,
};
use kumochi_protocol::profile::{Guard, Observation};
#[cfg(any(windows, target_os = "macos"))]
use kumochi_protocol::profile::{Probes, Root};

use crate::connections::authority::existing::Legacy;
#[cfg(any(windows, target_os = "macos"))]
use crate::connections::authority::existing::legacy as legacy_authority;
use crate::platform::credential::CredentialStore;
use crate::platform::{FolderId, SessionLock, folder_id};
#[cfg(any(windows, target_os = "macos"))]
use crate::platform::{credential::NativeCredentialStore, profile as native};
use crate::store::existing::{CURRENT_HISTORY, LegacyHistory, legacy_history};

/// What the noncreating lookup found for one legacy product folder.
pub(crate) enum Folder {
    /// A definitive not-found of the exact name under its held, verified anchor.
    Missing,
    /// A trusted folder at this path, held, with the identity of what the held handle is open on.
    Present(PathBuf, FolderId),
    /// No verified anchor, or anything else at the name.
    Unknown,
}

/// The probes over this machine's Known Folders (Application Support on macOS), setup guard and
/// credential store. Each probe holds its anchors through its reads.
#[cfg(any(windows, target_os = "macos"))]
#[cfg_attr(
    test,
    expect(dead_code, reason = "tests never read the person's real profile")
)]
pub(crate) struct NativeProbes;

#[cfg(any(windows, target_os = "macos"))]
impl Probes for NativeProbes {
    type Guard = native::HeldGuard;

    fn guard(&self) -> Guard<Self::Guard> {
        taken(native::setup_guard(), &native::GuardError::Busy)
    }

    fn legacy(&self, guard: &Self::Guard) -> Observation {
        let folders = native::legacy(guard);
        legacy(&folders.data, folders.roaming.as_ref(), |data, held| {
            intact(data, held, |db, session| {
                legacy_authority(db, session, &NativeCredentialStore)
            })
        })
    }

    fn root(&self, guard: &Self::Guard) -> Root {
        native::profile_root(guard)
    }

    fn owner(&self, _: &Self::Guard) -> Observation {
        owner(&NativeCredentialStore)
    }
}

/// The guard as `observe` takes it: only contention is busy, and any other failure is unavailable.
fn taken<G, E: PartialEq>(result: Result<G, E>, busy: &E) -> Guard<G> {
    match result {
        Ok(guard) => Guard::Held(guard),
        Err(error) if error == *busy => Guard::Busy,
        Err(_) => Guard::Unavailable,
    }
}

/// Legacy footprints from the product folders' lookups. Absent only when each is a definitive
/// not-found; read in place only when the local data folder is present and no lookup failed. A
/// roaming folder without local data is a remnant.
fn legacy(
    data: &Folder,
    roaming: Option<&Folder>,
    intact: impl FnOnce(&Path, FolderId) -> Observation,
) -> Observation {
    match (data, roaming) {
        (Folder::Missing, None | Some(Folder::Missing)) => Observation::Absent,
        (Folder::Present(data, held), None | Some(Folder::Missing | Folder::Present(..))) => {
            intact(data, *held)
        }
        _ => Observation::Unverified,
    }
}

/// The legacy profile in the present folder `data`, read under its existing session lock, which
/// stays held through every read. Nothing is opened unless the folder at `data` is the admitted
/// one, `held`, which it must still be after the last read. Matching only with the current history
/// schema and an authority and committed host that both match. Anything else is unverified: an
/// absent authority or host too, since the folder is there, and an older known schema, whose
/// migration waits on the owner's call on legacy schema updates.
fn intact(
    data: &Path,
    held: FolderId,
    authority: impl FnOnce(&Path, &SessionLock) -> Legacy,
) -> Observation {
    let admitted = || folder_id(data) == Some(held);
    if !admitted() {
        return Observation::Unverified;
    }
    let Ok(session) = SessionLock::try_acquire_existing(&data.join(SESSION_LOCK)) else {
        return Observation::Unverified;
    };
    let read = match legacy_history(&data.join(STORE_FILE), &session) {
        LegacyHistory::KnownSchema(CURRENT_HISTORY) => {
            match authority(&data.join(AUTHORITY_FILE), &session) {
                Legacy {
                    authority: Observation::Matching,
                    host: Observation::Matching,
                } => Observation::Matching,
                Legacy { .. } => Observation::Unverified,
            }
        }
        LegacyHistory::KnownSchema(_) | LegacyHistory::Missing | LegacyHistory::Unverified => {
            Observation::Unverified
        }
    };
    if admitted() {
        read
    } else {
        Observation::Unverified
    }
}

/// Reads this edition's owner item by its exact name, creating, changing and removing nothing.
/// Only a missing item is absent. Before the allocator writes its own, an item there is of
/// unknown origin, so its bytes are dropped unread; a failed read proves nothing either.
pub(crate) fn owner(store: &impl CredentialStore) -> Observation {
    let target = account_target(
        ProductIdentity::ACTIVE.profile_service,
        PROFILE_OWNER_ACCOUNT,
    );
    match store.get(&target) {
        Ok(None) => Observation::Absent,
        Ok(Some(_)) | Err(_) => Observation::Unverified,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use kumochi_protocol::Secret;
    use zeroize::Zeroizing;

    use super::*;
    use crate::connections::authority::existing::legacy as legacy_authority;
    use crate::connections::vault::fake::FakeStore;
    use crate::store::Store;
    use crate::test_support::{Scratch, files};

    #[derive(Debug, PartialEq)]
    enum Failure {
        Busy,
        Unavailable,
        Io,
    }

    #[test]
    fn only_contention_is_a_busy_guard() {
        assert!(matches!(
            taken(Ok::<_, Failure>(7), &Failure::Busy),
            Guard::Held(7)
        ));
        assert!(matches!(
            taken(Err::<(), _>(Failure::Busy), &Failure::Busy),
            Guard::Busy
        ));
        for failure in [Failure::Unavailable, Failure::Io] {
            assert!(matches!(
                taken(Err::<(), _>(failure), &Failure::Busy),
                Guard::Unavailable
            ));
        }
    }

    #[test]
    fn legacy_is_absent_only_when_every_folder_is_missing() {
        let id = folder_id(&std::env::temp_dir()).expect("a folder's identity");
        let present = || Folder::Present(PathBuf::from("data"), id);
        let cases = [
            (Folder::Missing, None, Observation::Absent),
            (Folder::Missing, Some(Folder::Missing), Observation::Absent),
            (Folder::Missing, Some(present()), Observation::Unverified),
            (
                Folder::Missing,
                Some(Folder::Unknown),
                Observation::Unverified,
            ),
            (Folder::Unknown, None, Observation::Unverified),
            (
                Folder::Unknown,
                Some(Folder::Missing),
                Observation::Unverified,
            ),
            (present(), None, Observation::Matching),
            (present(), Some(Folder::Missing), Observation::Matching),
            (present(), Some(present()), Observation::Matching),
            (present(), Some(Folder::Unknown), Observation::Unverified),
        ];
        for (case, (data, roaming, expected)) in cases.into_iter().enumerate() {
            let mut read = None;
            let found = legacy(&data, roaming.as_ref(), |data, held| {
                assert_eq!(held, id, "case {case}");
                read = Some(data.to_owned());
                Observation::Matching
            });
            assert_eq!(found, expected, "case {case}");
            assert_eq!(
                read.is_some(),
                expected == Observation::Matching,
                "case {case}"
            );
        }
    }

    const MATCHING: Legacy = Legacy {
        authority: Observation::Matching,
        host: Observation::Matching,
    };

    /// A legacy data folder at `data` with an empty session lock and a history store the brain
    /// opened.
    fn profile(data: PathBuf) -> PathBuf {
        drop(Store::open(&data.join(STORE_FILE)).expect("a history store"));
        fs::write(data.join(SESSION_LOCK), b"").expect("the session lock");
        data
    }

    /// `intact` on the folder at `data` as admitted now.
    fn read_admitted(
        data: &Path,
        authority: impl FnOnce(&Path, &SessionLock) -> Legacy,
    ) -> Observation {
        let held = folder_id(data).expect("an admitted folder");
        intact(data, held, authority)
    }

    #[test]
    fn a_folder_swapped_after_admission_is_unverified() {
        let scratch = Scratch::new("profile-swapped");
        let data = profile(scratch.dir().join("data"));
        let other = profile(scratch.dir().join("other"));
        let admitted = scratch.dir().join("admitted");
        let held = folder_id(&data).expect("an admitted folder");
        let swap = || {
            fs::rename(&data, &admitted).expect("move the admitted folder away");
            fs::rename(&other, &data).expect("put another profile in its place");
        };
        swap();
        let called = Cell::new(false);
        let before = intact(&data, held, |_, _| {
            called.set(true);
            MATCHING
        });
        assert_eq!(before, Observation::Unverified, "swapped before the read");
        assert!(!called.get(), "nothing in the other folder was read");
        fs::rename(&data, &other).expect("move the other profile back");
        fs::rename(&admitted, &data).expect("restore the admitted folder");
        assert_eq!(
            intact(&data, held, |_, _| MATCHING),
            Observation::Matching,
            "the admitted folder in place"
        );
        let during = intact(&data, held, |_, _| {
            swap();
            MATCHING
        });
        assert_eq!(during, Observation::Unverified, "swapped during the reads");
    }

    #[test]
    fn an_intact_legacy_profile_matches_only_with_a_matching_authority() {
        let scratch = Scratch::new("profile-intact");
        let data = profile(scratch.dir().to_owned());
        let before = files(&data);
        let read = read_admitted(&data, |db, _| {
            assert_eq!(db, data.join(AUTHORITY_FILE));
            assert!(
                matches!(
                    SessionLock::try_acquire_existing(&data.join(SESSION_LOCK)),
                    Err(crate::platform::SessionLockError::Busy)
                ),
                "the session lock is held through the authority read"
            );
            MATCHING
        });
        assert_eq!(read, Observation::Matching);
        let (absent, unverified) = (Observation::Absent, Observation::Unverified);
        for (authority, host) in [
            (absent, absent),
            (absent, Observation::Matching),
            (unverified, unverified),
            (unverified, Observation::Matching),
            (Observation::Matching, unverified),
        ] {
            assert_eq!(
                read_admitted(&data, |_, _| Legacy { authority, host }),
                Observation::Unverified,
                "{authority:?} authority, {host:?} host"
            );
        }
        let store = FakeStore::default();
        assert_eq!(
            read_admitted(&data, |db, session| legacy_authority(db, session, &store)),
            Observation::Unverified,
            "the real step finds no authority, which a present folder can't have"
        );
        assert!(store.names().is_empty());
        assert_eq!(files(&data), before);
    }

    #[test]
    fn a_matching_authority_without_its_host_is_unverified() {
        let scratch = Scratch::new("profile-no-host");
        let data = profile(scratch.dir().to_owned());
        let no_host = Legacy {
            authority: Observation::Matching,
            host: Observation::Absent,
        };
        assert_eq!(
            read_admitted(&data, |_, _| no_host),
            Observation::Unverified
        );
    }

    #[test]
    fn only_the_current_history_version_can_match() {
        let scratch = Scratch::new("profile-history-versions");
        for version in 1..=CURRENT_HISTORY {
            let data = scratch.dir().join(format!("v{version}"));
            let db = crate::store::existing::tests::store_at(&data, version);
            fs::rename(db, data.join(STORE_FILE)).expect("the history store's name");
            fs::write(data.join(SESSION_LOCK), b"").expect("the session lock");
            let before = files(&data);
            let read = read_admitted(&data, |_, _| MATCHING);
            let expected = if version == CURRENT_HISTORY {
                Observation::Matching
            } else {
                Observation::Unverified
            };
            assert_eq!(read, expected, "v{version}");
            assert_eq!(files(&data), before, "v{version}");
            let session = SessionLock::try_acquire_existing(&data.join(SESSION_LOCK))
                .expect("the session lock is free again");
            assert_eq!(
                legacy_history(&data.join(STORE_FILE), &session),
                LegacyHistory::KnownSchema(version),
                "v{version} is a known schema"
            );
        }
    }

    #[test]
    fn a_legacy_profile_without_its_lock_or_store_is_unverified_and_kept() {
        let scratch = Scratch::new("profile-partial");
        let data = profile(scratch.dir().to_owned());
        let matching = |_: &Path, _: &SessionLock| MATCHING;
        let held = SessionLock::try_acquire_existing(&data.join(SESSION_LOCK)).expect("held");
        assert_eq!(
            read_admitted(&data, matching),
            Observation::Unverified,
            "busy"
        );
        drop(held);
        fs::remove_file(data.join(SESSION_LOCK)).expect("no lock");
        let before = files(&data);
        assert_eq!(
            read_admitted(&data, matching),
            Observation::Unverified,
            "no lock"
        );
        assert_eq!(files(&data), before, "looking created no lock");
        fs::write(data.join(SESSION_LOCK), b"").expect("the session lock");
        fs::remove_file(data.join(STORE_FILE)).expect("no store");
        let before = files(&data);
        assert_eq!(
            read_admitted(&data, matching),
            Observation::Unverified,
            "no store"
        );
        assert_eq!(files(&data), before);
    }

    /// The owner item's exact name in the edition this test build compiled.
    fn owner_target() -> &'static str {
        match ProductIdentity::ACTIVE {
            identity if identity == ProductIdentity::GATE => "Kumochi Gate Profile/owner",
            identity if identity == ProductIdentity::DEV => "Kumochi Dev Profile/owner",
            _ => "Kumochi Profile/owner",
        }
    }

    #[test]
    fn a_missing_owner_item_is_absent() {
        let store = FakeStore::default();
        store.0.lock().expect("fake lock").values.insert(
            "Kumochi Dev/owner".to_owned(),
            Secret::new(Zeroizing::new(vec![1])),
        );
        assert_eq!(owner(&store), Observation::Absent);
        assert_eq!(store.names(), ["Kumochi Dev/owner"]);
        assert!(store.0.lock().expect("fake lock").mutations.is_empty());
    }

    #[test]
    fn a_present_owner_item_is_unverified_and_left_as_it_was() {
        let store = FakeStore::default();
        store.0.lock().expect("fake lock").values.insert(
            owner_target().to_owned(),
            Secret::new(Zeroizing::new(b"owner-sentinel".to_vec())),
        );
        assert_eq!(owner(&store), Observation::Unverified);
        let state = store.0.lock().expect("fake lock");
        assert!(state.mutations.is_empty());
        assert_eq!(state.values.len(), 1);
        assert_eq!(
            state.values[owner_target()].expose().as_slice(),
            b"owner-sentinel"
        );
    }

    #[test]
    fn a_refused_read_is_unverified() {
        let store = FakeStore::default();
        store.0.lock().expect("fake lock").denied_read = Some(owner_target().to_owned());
        assert_eq!(owner(&store), Observation::Unverified);
        let state = store.0.lock().expect("fake lock");
        assert!(state.mutations.is_empty());
        assert!(state.values.is_empty());
    }
}
