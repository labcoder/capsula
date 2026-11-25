//! The exact-name contract every native `CredentialStore` meets, run by each platform's tests.

use std::thread;

use kumochi_protocol::Secret;
use zeroize::Zeroizing;

use super::{CredentialStore, CredentialStoreError, SecretBytes};

fn secret(bytes: &[u8]) -> SecretBytes {
    Secret::new(Zeroizing::new(bytes.to_vec()))
}

fn read(store: &impl CredentialStore, target: &str) -> Vec<u8> {
    store
        .get(target)
        .expect("read")
        .expect("credential exists")
        .expose()
        .to_vec()
}

/// Bad names and oversized blobs are refused before the OS sees them, so nothing is written.
pub(crate) fn refuses_invalid_input(store: &impl CredentialStore, max_blob: usize, target: &str) {
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
            store.set(target, &secret(&vec![42; max_blob + 1])),
            Err(CredentialStoreError::BlobTooLarge)
        ),
        "one byte over the limit is refused"
    );
}

/// Absence, empty versus missing, byte-exact replacement, the maximum blob, a read on another
/// thread and idempotent deletion, all on one exact owned name.
pub(crate) fn round_trip(store: &impl CredentialStore, target: &str, max_blob: usize) {
    assert!(
        store.get(target).expect("missing read").is_none(),
        "missing is absence"
    );
    store.delete(target).expect("missing delete succeeds");

    store.set(target, &secret(&[])).expect("empty write");
    assert_eq!(
        read(store, target),
        b"",
        "an empty blob is present, not missing"
    );

    store
        .set(target, &secret(b"test-only-longer-sentinel"))
        .expect("first write");
    store
        .set(target, &secret(b"short"))
        .expect("replacing write");
    assert_eq!(
        read(store, target),
        b"short",
        "replacement leaves no old tail"
    );

    let max = vec![42; max_blob];
    store.set(target, &secret(&max)).expect("maximum write");
    thread::scope(|scope| {
        let other = scope.spawn(|| read(store, target));
        assert_eq!(
            other.join().expect("reader thread"),
            max,
            "read on another thread"
        );
    });

    store.delete(target).expect("delete existing");
    assert!(
        store.get(target).expect("read after delete").is_none(),
        "deleted is absent"
    );
    store.delete(target).expect("delete again");
}
