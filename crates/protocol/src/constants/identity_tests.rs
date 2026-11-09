use super::*;
use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn executable_names_resolve_only_the_native_sibling() {
    #[cfg(target_os = "macos")]
    let (shell, brain, shell_path, brain_path) =
        ("kumochi", "kumochid", "/app/kumochi", "/app/kumochid");
    #[cfg(not(target_os = "macos"))]
    let (shell, brain, shell_path, brain_path) = (
        "kumochi.exe",
        "kumochid.exe",
        "C:/app/kumochi.exe",
        "C:/app/kumochid.exe",
    );
    assert_eq!(SHELL_EXE, shell);
    assert_eq!(BRAIN_EXE, brain);
    assert_eq!(
        Path::new(shell_path).with_file_name(BRAIN_EXE),
        Path::new(brain_path)
    );
    assert_eq!(
        Path::new(brain_path).with_file_name(SHELL_EXE),
        Path::new(shell_path)
    );
}

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
    #[cfg(debug_assertions)]
    let expected = dev;
    #[cfg(not(debug_assertions))]
    let expected = release;
    assert_eq!(ProductIdentity::ACTIVE, expected);
    assert_eq!(DATA_DIR, expected.data_dir);
    assert_eq!(CREDENTIAL_SERVICE, expected.credential_service);
    assert_eq!(INSTANCE_SERVICE, expected.instance_service);
    assert_eq!(OBJECT_PREFIX, expected.object_prefix);
    assert_eq!(SHELL_LOCK, expected.shell_lock);
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
        assert_eq!(set.len(), 262);
        assert_eq!(set, expected);
        set
    };
    let release = inventory("Kumochi");
    let dev = inventory("Kumochi Dev");
    assert!(release.is_disjoint(&dev));
    assert!(release.contains("Kumochi/chatgpt/0/0"));
    assert!(release.contains("Kumochi/revoke/1/63"));
    assert!(!release.contains("Kumochi/revoke/1/64"));
    assert!(!release.contains("Kumochi/revoke/2/0"));
    assert_eq!(account_target("Kumochi", "host"), "Kumochi/host");
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
        assert_eq!(parse_instance_entry(account), Some(session));
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

#[test]
fn instance_parser_rejects_noncanonical_and_overflow_accounts() {
    for account in [
        "",
        "capabilit",
        "capability",
        "capability-",
        "capability-4294967296",
        "capability-99999999999",
        "capability-01x",
        "capability-01",
        "capability-00",
        "Capability-1",
        "capability-+1",
        "capability--1",
        "capability- 1",
        "capability-1 ",
        " capability-1",
        "capability-١",
        "capability-1\n",
        "capability-7.Kumochi instance",
        "capability-7.Kumochi Dev instance",
    ] {
        assert_eq!(parse_instance_entry(account), None, "{account:?}");
    }
}
