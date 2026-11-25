//! Read-only probes behind the shared profile observer.

use kumochi_protocol::constants::{PROFILE_OWNER_ACCOUNT, ProductIdentity, account_target};
use kumochi_protocol::profile::Observation;

use crate::platform::credential::CredentialStore;

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
    use kumochi_protocol::Secret;
    use zeroize::Zeroizing;

    use super::*;
    use crate::connections::vault::fake::FakeStore;

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
