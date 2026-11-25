use objc2_security::{errSecInteractionNotAllowed, errSecMissingEntitlement};

use crate::platform::credential::conformance;
use crate::platform::credential::fixture::Fixture;

use super::*;

const GROUP: &str = "TESTTEAM.io.example.kumochi-test";

fn store() -> MacCredentialStore {
    MacCredentialStore {
        group: GROUP.to_owned(),
    }
}

fn scratch_name() -> String {
    let nonce = getrandom::u64().expect("random test name");
    format!("{CREDENTIAL_SERVICE}-credential-test-{nonce:016x}")
}

fn text(query: &Query, key: &CFString) -> Option<String> {
    query
        .get(key)
        .and_then(|value| value.downcast::<CFString>().ok())
        .map(|value| value.to_string())
}

fn flag(query: &Query, key: &CFString) -> Option<bool> {
    query
        .get(key)
        .and_then(|value| value.downcast::<CFBoolean>().ok())
        .map(|value| value.as_bool())
}

/// The context's address, after checking it refuses interaction.
fn no_ui_context_in(query: &Query) -> *const AnyObject {
    let context = query
        .get(constant!(kSecUseAuthenticationContext))
        .expect("every query carries a context");
    let object: *const CFType = &raw const *context;
    // SAFETY: the only value under this key is the LAContext the query builder placed there,
    // kept alive by `context` for this borrow.
    let object = unsafe { &*object.cast::<AnyObject>() };
    assert!(interaction_refused(object), "the context refuses UI");
    ptr::from_ref(object)
}

/// Checked against the SDK's own constants, not the production key table.
fn assert_names_one_item(query: &Query, target: &str) {
    assert_eq!(
        text(query, constant!(kSecClass)),
        Some(constant!(kSecClassGenericPassword).to_string())
    );
    assert_eq!(
        text(query, constant!(kSecAttrAccessGroup)).as_deref(),
        Some(GROUP)
    );
    assert_eq!(
        text(query, constant!(kSecAttrService)).as_deref(),
        Some(CREDENTIAL_SERVICE)
    );
    assert_eq!(
        text(query, constant!(kSecAttrAccount)).as_deref(),
        Some(target)
    );
    assert_eq!(
        flag(query, constant!(kSecUseDataProtectionKeychain)),
        Some(true)
    );
    assert_eq!(flag(query, constant!(kSecAttrSynchronizable)), Some(false));
}

#[test]
fn every_query_names_one_item_in_the_pinned_group_without_ui() {
    let store = store();
    let target = "test-only-target";
    let data = CFData::from_bytes(b"test-only-data");

    let read = store.read_query(target).expect("read query");
    assert_names_one_item(&read, target);
    assert_eq!(flag(&read, constant!(kSecReturnData)), Some(true));
    assert_eq!(
        text(&read, constant!(kSecMatchLimit)),
        Some(constant!(kSecMatchLimitOne).to_string())
    );
    assert!(
        !read.contains_key(constant!(kSecValueData)),
        "a read sends no data"
    );

    let add = store.add_attributes(target, &data).expect("add attributes");
    assert_names_one_item(&add, target);
    assert_eq!(
        text(&add, constant!(kSecAttrAccessible)),
        Some(constant!(kSecAttrAccessibleWhenUnlockedThisDeviceOnly).to_string())
    );
    assert!(
        add.contains_key(constant!(kSecValueData)),
        "an add carries the data"
    );
    assert!(
        !add.contains_key(constant!(kSecReturnData)),
        "an add returns nothing"
    );

    let exact = store.query(target, &[]).expect("identity query");
    assert_names_one_item(&exact, target);
    assert_eq!(
        exact.len(),
        7,
        "update and delete match only the item's key and context"
    );

    let update = update_attributes(&data);
    assert_eq!(update.len(), 1, "an update changes only the data");
    assert!(
        update.contains_key(constant!(kSecValueData)),
        "an update carries the data"
    );

    let contexts = [&read, &add, &exact].map(no_ui_context_in);
    assert!(
        contexts[0] != contexts[1] && contexts[1] != contexts[2] && contexts[0] != contexts[2],
        "every call gets its own context"
    );
}

#[test]
fn a_write_view_holds_an_exact_copy_of_the_bytes() {
    let view = owned_view(b"test-only-view-sentinel").expect("owned view");
    assert!(
        view.to_vec() == b"test-only-view-sentinel",
        "the view reads the copied bytes"
    );
    let empty = owned_view(&[]).expect("empty view");
    assert_eq!(empty.length(), 0);
}

#[test]
fn refusals_stay_operation_errors_and_only_not_found_is_absence() {
    assert!(matches!(found(errSecSuccess), Ok(true)));
    assert!(matches!(found(errSecItemNotFound), Ok(false)));
    assert!(matches!(
        found(errSecInteractionNotAllowed),
        Err(CredentialStoreError::Read(-25_308))
    ));
    assert!(matches!(
        found(errSecMissingEntitlement),
        Err(CredentialStoreError::Read(-34_018))
    ));
    assert!(
        matches!(
            succeeded(errSecItemNotFound, CredentialStoreError::Write),
            Err(CredentialStoreError::Write(-25_300))
        ),
        "an update racing a delete is a write error, not success or a retry"
    );
    assert!(matches!(
        succeeded(errSecInteractionNotAllowed, CredentialStoreError::Delete),
        Err(CredentialStoreError::Delete(-25_308))
    ));
}

#[test]
fn names_are_bounded_in_utf16_units() {
    let at_limit = format!("{}x", "\u{1F600}".repeat(16_383));
    assert_eq!(at_limit.encode_utf16().count(), 32_767);
    assert!(validate(&at_limit).is_ok(), "exactly the limit is accepted");
    let over = format!("{at_limit}x");
    assert!(matches!(
        validate(&over),
        Err(CredentialStoreError::InvalidTarget)
    ));
    assert!(matches!(
        validate(""),
        Err(CredentialStoreError::InvalidTarget)
    ));
    assert!(matches!(
        validate("a\0b"),
        Err(CredentialStoreError::InvalidTarget)
    ));
}

#[test]
fn invalid_input_is_refused_before_the_keychain() {
    conformance::refuses_invalid_input(&store(), &scratch_name());
}

#[test]
#[ignore = "needs a test host signed and provisioned for the data protection keychain"]
fn a_signed_host_meets_the_shared_contract() {
    let store = MacCredentialStore::admitted().expect("a provisioned application identifier");
    let (target, other) = (scratch_name(), scratch_name());
    let _fixture = Fixture::with_store(store.clone(), vec![target.clone(), other.clone()]);
    conformance::round_trip(&store, &target, &other);
}
