//! Compiles the shell's real staging entry point against the brain's native vault for this check.

#[path = "../../../../../shell/src/platform/credential.rs"]
mod shell_staging;

use super::*;
use crate::connections::anthropic::KeyOp;
use crate::platform::credential::{NativeCredentialStore, fixture::Fixture};
use kumochi_protocol::constants::{
    ProductIdentity, anthropic_key_target, anthropic_staging_target,
};

#[test]
#[ignore = "Rez only: writes a dummy API key under the Dev credential names"]
fn dev_keyring_staging_round_trip() {
    assert_eq!(ProductIdentity::ACTIVE, ProductIdentity::DEV);
    let service = ProductIdentity::DEV.credential_service;
    let active = anthropic_key_target(service);
    let staged = anthropic_staging_target(service);
    let store = NativeCredentialStore;
    for _ in 0..4 {
        for target in [&active, &staged] {
            assert_eq!(
                store.get(target).expect("check Dev name is unused"),
                None,
                "never overwrite an existing Dev key"
            );
        }
    }
    let _cleanup = Fixture::new(vec![active.clone(), staged.clone()]);
    let scratch = Scratch::new("anthropic-dev-roundtrip");
    let lock = scratch.db().with_file_name(SESSION_LOCK);
    let mut vault = Vault::with_service(store, service, &lock).expect("test session lock");
    let dummy = "sk-ant-api-dummy-roundtrip-not-a-real-key";
    shell_staging::stage_anthropic_key(Secret::new(Zeroizing::new(format!(" \t{dummy}\r\n"))))
        .expect("shell stages dummy");
    let adopted = vault
        .anthropic(KeyOp::Adopt)
        .expect("vault adopts shell staging")
        .expect("dummy key");
    assert_eq!(adopted.expose().as_str(), dummy);
    assert_eq!(
        NativeCredentialStore.get(&staged).expect("staging absent"),
        None
    );
    assert_eq!(
        NativeCredentialStore
            .get(&active)
            .expect("active record")
            .expect("saved dummy")
            .expose()
            .as_slice(),
        dummy.as_bytes()
    );
    assert_eq!(
        vault
            .anthropic(KeyOp::Remove)
            .expect("remove dummy")
            .map(|_| ()),
        None
    );
    assert_eq!(
        NativeCredentialStore.get(&active).expect("active absent"),
        None
    );
    assert_eq!(
        NativeCredentialStore.get(&staged).expect("staging absent"),
        None
    );
}
