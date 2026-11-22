use super::*;
use std::collections::BTreeSet;

#[test]
fn identities_preserve_stored_names_and_profile_aliases() {
    let release = ProductIdentity::RELEASE;
    let dev = ProductIdentity::DEV;
    assert_eq!(
        [
            release.data_dir,
            release.credential_service,
            release.instance_service,
            release.object_prefix,
            release.shell_lock
        ],
        [
            "Kumochi",
            "Kumochi",
            "Kumochi instance",
            "kumochi",
            "kumochi-shell"
        ]
    );
    assert_eq!(
        [
            dev.data_dir,
            dev.credential_service,
            dev.instance_service,
            dev.object_prefix,
            dev.shell_lock
        ],
        [
            "Kumochi Dev",
            "Kumochi Dev",
            "Kumochi Dev instance",
            "kumochi-dev",
            "kumochi-dev-shell"
        ]
    );
    let expected = match (cfg!(feature = "gate-identity"), cfg!(debug_assertions)) {
        (true, _) => ProductIdentity::GATE,
        (false, true) => dev,
        (false, false) => release,
    };
    assert_eq!(ProductIdentity::ACTIVE, expected);
    assert_eq!(DATA_DIR, expected.data_dir);
    assert_eq!(CREDENTIAL_SERVICE, expected.credential_service);
    assert_eq!(INSTANCE_SERVICE, expected.instance_service);
    assert_eq!(OBJECT_PREFIX, expected.object_prefix);
    assert_eq!(SHELL_LOCK, expected.shell_lock);
}

#[test]
fn the_gate_identity_shares_no_stored_name_with_the_real_ones() {
    let gate = ProductIdentity::GATE;
    assert_eq!(
        [
            gate.data_dir,
            gate.credential_service,
            gate.instance_service,
            gate.object_prefix,
            gate.shell_lock
        ],
        [
            "Kumochi Gate",
            "Kumochi Gate",
            "Kumochi Gate instance",
            "kumochi-gate",
            "kumochi-gate-shell"
        ]
    );
    assert_eq!(
        instance_target(gate.instance_service, 7),
        "capability-7.Kumochi Gate instance"
    );
    assert_eq!(
        generation_target(gate.credential_service, "host", "00ff", 0),
        "Kumochi Gate/gen/host/00ff/0"
    );
    let targets: BTreeSet<_> = account_targets(gate.credential_service).collect();
    assert_eq!(targets.len(), 262);
    for real in [ProductIdentity::RELEASE, ProductIdentity::DEV] {
        assert!(targets.is_disjoint(&account_targets(real.credential_service).collect()));
    }
}

#[test]
fn account_inventory_is_exact_bounded_and_disjoint() {
    let inventory = |service: &str| {
        let actual: Vec<_> = account_targets(service).collect();
        let set: BTreeSet<_> = actual.iter().cloned().collect();
        let mut expected: BTreeSet<_> = ["host", "signout", "chatgpt", "revoke"]
            .map(|name| format!("{service}/{name}"))
            .into_iter()
            .collect();
        for record in ["chatgpt", "revoke"] {
            for slot in 0..2 {
                for index in 0..64 {
                    expected.insert(format!("{service}/{record}/{slot}/{index}"));
                }
            }
        }
        expected.insert(format!("anthropic-key.{service}"));
        expected.insert(format!("anthropic-key-staged.{service}"));
        assert_eq!(actual.len(), 262);
        assert_eq!(set, expected);
        set
    };
    let release = inventory("Kumochi");
    let dev = inventory("Kumochi Dev");
    assert!(release.is_disjoint(&dev));
    assert_eq!(anthropic_key_target("Kumochi"), "anthropic-key.Kumochi");
    assert_eq!(
        anthropic_staging_target("Kumochi"),
        "anthropic-key-staged.Kumochi"
    );
    assert_eq!(
        anthropic_key_target("Kumochi Dev"),
        "anthropic-key.Kumochi Dev"
    );
    assert_eq!(
        anthropic_staging_target("Kumochi Dev"),
        "anthropic-key-staged.Kumochi Dev"
    );
}

#[test]
fn instance_accounts_and_windows_targets_keep_their_spelling() {
    for (session, account) in [
        (0, "capability-0"),
        (1, "capability-1"),
        (u32::MAX, "capability-4294967295"),
    ] {
        assert_eq!(instance_entry(session), account);
    }
    assert_eq!(
        instance_target(ProductIdentity::RELEASE.instance_service, 7),
        "capability-7.Kumochi instance"
    );
    assert_eq!(
        instance_target(ProductIdentity::DEV.instance_service, 7),
        "capability-7.Kumochi Dev instance"
    );
}
