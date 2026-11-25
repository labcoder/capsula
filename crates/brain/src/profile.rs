//! Read-only probes behind the shared profile observer.

use std::path::Path;

use kumochi_protocol::constants::{
    PROFILE_OWNER_ACCOUNT, ProductIdentity, SESSION_LOCK, STORE_FILE, account_target,
};
use kumochi_protocol::profile::Observation;

use crate::platform::SessionLock;
use crate::platform::credential::CredentialStore;
use crate::store::existing::{LegacyHistory, legacy_history};

/// The legacy profile in the present folder `data`, read under its existing session lock, which
/// stays held through every read. Matching only with a known history schema and `authority`'s
/// match; anything else, an absent authority included, is unverified.
fn intact(data: &Path, authority: impl FnOnce(&Path, &SessionLock) -> Observation) -> Observation {
    let Ok(session) = SessionLock::try_acquire_existing(&data.join(SESSION_LOCK)) else {
        return Observation::Unverified;
    };
    match legacy_history(&data.join(STORE_FILE), &session) {
        LegacyHistory::KnownSchema => match authority(data, &session) {
            Observation::Matching => Observation::Matching,
            Observation::Absent | Observation::Unverified => Observation::Unverified,
        },
        LegacyHistory::Missing | LegacyHistory::Unverified => Observation::Unverified,
    }
}

/// The legacy authority in `data` and its committed host, read under `session`.
///
/// shortcut: always unverified, so a present legacy folder never matches, until the read-only
/// authority and host checks land; wiring them in replaces this body with their call.
fn authority_and_host(_data: &Path, _session: &SessionLock) -> Observation {
    Observation::Unverified
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
    use std::fs;
    use std::path::PathBuf;

    use kumochi_protocol::Secret;
    use zeroize::Zeroizing;

    use super::*;
    use crate::connections::vault::fake::FakeStore;
    use crate::store::Store;
    use crate::test_support::{Scratch, files};

    /// A legacy data folder with an empty session lock and a history store the brain opened.
    fn profile(scratch: &Scratch) -> PathBuf {
        let data = scratch.dir().to_owned();
        drop(Store::open(&data.join(STORE_FILE)).expect("a history store"));
        fs::write(data.join(SESSION_LOCK), b"").expect("the session lock");
        data
    }

    #[test]
    fn an_intact_legacy_profile_matches_only_with_a_matching_authority() {
        let scratch = Scratch::new("profile-intact");
        let data = profile(&scratch);
        let before = files(&data);
        let read = intact(&data, |dir, _| {
            assert_eq!(dir, data);
            assert!(
                matches!(
                    SessionLock::try_acquire_existing(&data.join(SESSION_LOCK)),
                    Err(crate::platform::SessionLockError::Busy)
                ),
                "the session lock is held through the authority read"
            );
            Observation::Matching
        });
        assert_eq!(read, Observation::Matching);
        for verdict in [Observation::Absent, Observation::Unverified] {
            assert_eq!(intact(&data, |_, _| verdict), Observation::Unverified);
        }
        assert_eq!(
            intact(&data, authority_and_host),
            Observation::Unverified,
            "no authority check is wired in yet"
        );
        assert_eq!(files(&data), before);
    }

    #[test]
    fn a_legacy_profile_without_its_lock_or_store_is_unverified_and_kept() {
        let scratch = Scratch::new("profile-partial");
        let data = profile(&scratch);
        let matching = |_: &Path, _: &SessionLock| Observation::Matching;
        let held = SessionLock::try_acquire_existing(&data.join(SESSION_LOCK)).expect("held");
        assert_eq!(intact(&data, matching), Observation::Unverified, "busy");
        drop(held);
        fs::remove_file(data.join(SESSION_LOCK)).expect("no lock");
        let before = files(&data);
        assert_eq!(intact(&data, matching), Observation::Unverified, "no lock");
        assert_eq!(files(&data), before, "looking created no lock");
        fs::write(data.join(SESSION_LOCK), b"").expect("the session lock");
        fs::remove_file(data.join(STORE_FILE)).expect("no store");
        let before = files(&data);
        assert_eq!(intact(&data, matching), Observation::Unverified, "no store");
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
