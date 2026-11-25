//! The exact-name contract every native `CredentialStore` meets, run by each platform's tests.

use std::thread;

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use super::{CredentialStore, CredentialStoreError, SecretBytes};

/// Every store's blob limit: Windows' generic credential maximum.
const MAX_BLOB: usize = 2_560;

fn secret(bytes: &[u8]) -> SecretBytes {
    Secret::new(Zeroizing::new(bytes.to_vec()))
}

/// Compares without printing: a wrong lookup could return someone's real credential.
fn holds(store: &impl CredentialStore, target: &str, expected: &[u8]) -> bool {
    let actual = store.get(target).expect("read").expect("credential exists");
    actual.expose().as_slice() == expected
}

/// Bad names and oversized blobs are refused before the OS sees them, so nothing is written.
pub(crate) fn refuses_invalid_input(store: &impl CredentialStore, target: &str) {
    let empty = secret(&[]);
    let long = "x".repeat(32_768);
    for invalid in ["", "test\0suffix", &long] {
        assert!(
            matches!(store.get(invalid), Err(CredentialStoreError::InvalidTarget)),
            "read refuses an invalid name"
        );
        assert!(
            matches!(
                store.set(invalid, &empty),
                Err(CredentialStoreError::InvalidTarget)
            ),
            "write refuses an invalid name"
        );
        assert!(
            matches!(
                store.delete(invalid),
                Err(CredentialStoreError::InvalidTarget)
            ),
            "delete refuses an invalid name"
        );
    }
    assert!(
        matches!(
            store.set(target, &secret(&[42; MAX_BLOB + 1])),
            Err(CredentialStoreError::BlobTooLarge)
        ),
        "one byte over the limit is refused"
    );
}

/// On two owned names: absence, empty versus missing, byte-exact replacement, the maximum blob,
/// a read on another thread and idempotent deletion, none of which touches the other name.
pub(crate) fn round_trip(store: &impl CredentialStore, target: &str, other: &str) {
    for name in [target, other] {
        assert!(
            store.get(name).expect("missing read").is_none(),
            "missing is absence"
        );
        store.delete(name).expect("missing delete succeeds");
    }
    let planted = b"test-only-other-sentinel";
    store
        .set(other, &secret(planted))
        .expect("write the other name");

    store.set(target, &secret(&[])).expect("empty write");
    assert!(
        holds(store, target, b""),
        "an empty blob is present, not missing"
    );

    store
        .set(target, &secret(b"test-only-longer-sentinel"))
        .expect("first write");
    store
        .set(target, &secret(b"short"))
        .expect("replacing write");
    assert!(
        holds(store, target, b"short"),
        "replacement leaves no old tail"
    );

    let max = [42; MAX_BLOB];
    store.set(target, &secret(&max)).expect("maximum write");
    thread::scope(|scope| {
        let other_thread = scope.spawn(|| holds(store, target, &max));
        assert!(
            other_thread.join().expect("reader thread"),
            "read on another thread"
        );
    });

    store.delete(target).expect("delete existing");
    assert!(
        store.get(target).expect("read after delete").is_none(),
        "deleted is absent"
    );
    store.delete(target).expect("delete again");
    assert!(holds(store, other, planted), "the other name is untouched");
    store.delete(other).expect("delete the other name");
}
