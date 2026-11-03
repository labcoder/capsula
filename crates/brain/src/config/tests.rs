use std::path::PathBuf;

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::message::{AssistantName, ErrorKey, SettingsOp, ThreadId, UserName};

use super::{Config, ConfigError};
use crate::test_support::Scratch;

fn dir(scratch: &Scratch) -> PathBuf {
    let dir = scratch.db().with_file_name("config");
    std::fs::create_dir_all(&dir).expect("config folder");
    dir
}

fn name(text: &str) -> AssistantName {
    AssistantName::new(text).expect("valid")
}

fn apply(config: &mut Config, op: SettingsOp) {
    let pending = config.stage(op).expect("staged");
    pending.write().expect("written");
    config.commit(pending);
}

#[test]
fn a_missing_file_gives_the_defaults_and_is_created_on_the_first_change() {
    let scratch = Scratch::new("config-missing");
    let dir = dir(&scratch).join("not yet");
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    let view = config.view();
    assert_eq!(view.assistant_name.as_str(), DEFAULT_ASSISTANT_NAME);
    assert_eq!(view.user_name, None);
    assert!(!view.onboarded);
    apply(&mut config, SettingsOp::SetAssistantName(name("Nimbus")));
    apply(&mut config, SettingsOp::FinishOnboarding);
    let reloaded = Config::load(Some(dir), ThreadId(1));
    assert_eq!(reloaded.view().assistant_name.as_str(), "Nimbus");
    assert!(reloaded.view().onboarded);
}

#[test]
fn unknown_keys_survive_a_write_and_clearing_the_user_name_removes_it() {
    let scratch = Scratch::new("config-unknown");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(
        &path,
        "future_key = 3\nuser_name = \"Oscar\"\n\n[later]\nmodel = \"x\"\n",
    )
    .expect("seeded");
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(
        config.view().user_name,
        Some(UserName::new("Oscar").expect("valid"))
    );
    apply(&mut config, SettingsOp::SetUserName(None));
    let text = std::fs::read_to_string(&path).expect("readable");
    let table: toml::Table = text.parse().expect("parses");
    assert_eq!(table.get("future_key"), Some(&toml::Value::Integer(3)));
    assert_eq!(table["later"]["model"].as_str(), Some("x"));
    assert!(!table.contains_key("user_name"));
    assert_eq!(Config::load(Some(dir), ThreadId(1)).view().user_name, None);
}

#[test]
fn an_unparseable_file_is_never_written_and_refuses_changes() {
    let scratch = Scratch::new("config-broken");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let broken = "assistant_name = \"Zqx\nnot toml [";
    std::fs::write(&path, broken).expect("seeded");
    let config = Config::load(Some(dir), ThreadId(1));
    assert_eq!(
        config.view().assistant_name.as_str(),
        DEFAULT_ASSISTANT_NAME
    );
    assert!(matches!(
        config.stage(SettingsOp::FinishOnboarding),
        Err(ConfigError::Unreadable)
    ));
    assert_eq!(std::fs::read_to_string(&path).expect("readable"), broken);
}

#[test]
fn an_unreadable_file_refuses_changes_like_a_broken_one() {
    let scratch = Scratch::new("config-unreadable");
    let dir = dir(&scratch);
    std::fs::create_dir(dir.join(CONFIG_FILE)).expect("a folder where the file goes");
    let config = Config::load(Some(dir), ThreadId(1));
    assert!(matches!(
        config.stage(SettingsOp::FinishOnboarding),
        Err(ConfigError::Unreadable)
    ));
}

#[test]
fn no_home_refuses_changes() {
    let config = Config::load(None, ThreadId(1));
    assert_eq!(
        config.view().assistant_name.as_str(),
        DEFAULT_ASSISTANT_NAME
    );
    assert!(matches!(
        config.stage(SettingsOp::FinishOnboarding),
        Err(ConfigError::NoHome)
    ));
}

#[test]
fn invalid_known_values_read_as_defaults_and_stay_until_set() {
    let scratch = Scratch::new("config-invalid");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let long = "a".repeat(25);
    std::fs::write(
        &path,
        format!("assistant_name = \"{long}\"\nuser_name = 5\nonboarded = \"yes\"\n"),
    )
    .expect("seeded");
    let mut config = Config::load(Some(dir), ThreadId(1));
    assert_eq!(
        config.view().assistant_name.as_str(),
        DEFAULT_ASSISTANT_NAME
    );
    assert_eq!(config.view().user_name, None);
    assert!(!config.view().onboarded);
    apply(&mut config, SettingsOp::FinishOnboarding);
    let table: toml::Table = std::fs::read_to_string(&path)
        .expect("readable")
        .parse()
        .expect("parses");
    assert_eq!(table["assistant_name"].as_str(), Some(long.as_str()));
    assert_eq!(table["user_name"].as_integer(), Some(5));
    assert_eq!(table["onboarded"].as_bool(), Some(true));
}

#[test]
fn a_failed_write_changes_neither_the_file_nor_the_settings() {
    let scratch = Scratch::new("config-failed");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    apply(&mut config, SettingsOp::SetAssistantName(name("Nimbus")));
    let before = std::fs::read_to_string(&path).expect("readable");
    // A folder where the temp file goes makes the write fail before the replace.
    std::fs::create_dir(dir.join(format!("{CONFIG_FILE}.tmp"))).expect("blocker");
    let pending = config
        .stage(SettingsOp::SetAssistantName(name("Cirrus")))
        .expect("staged");
    assert!(matches!(pending.write(), Err(ConfigError::Write(_))));
    drop(pending);
    assert_eq!(config.view().assistant_name.as_str(), "Nimbus");
    assert_eq!(std::fs::read_to_string(&path).expect("readable"), before);
    let staged = config.stage(SettingsOp::FinishOnboarding).expect("staged");
    assert_eq!(staged.view.assistant_name.as_str(), "Nimbus");
}

#[test]
fn debug_prints_neither_names_nor_unknown_values() {
    let scratch = Scratch::new("config-debug");
    let dir = dir(&scratch);
    std::fs::write(
        dir.join(CONFIG_FILE),
        "assistant_name = \"Zqxassistant\"\nfuture_key = \"Zqxunknown\"\n",
    )
    .expect("seeded");
    let config = Config::load(Some(dir), ThreadId(1));
    let pending = config
        .stage(SettingsOp::SetUserName(Some(
            UserName::new("Zqxuser").expect("valid"),
        )))
        .expect("staged");
    assert_eq!(
        format!("{config:?}"),
        "Config { loaded: true, keys: 2, .. }"
    );
    assert_eq!(format!("{pending:?}"), "Pending { keys: 3, .. }");
    let broken = Config::load(None, ThreadId(1));
    assert_eq!(
        format!("{broken:?}"),
        "Config { loaded: false, keys: 0, .. }"
    );
}

#[test]
fn only_an_unreadable_file_asks_the_user_to_fix_it() {
    assert_eq!(ConfigError::Unreadable.key(), ErrorKey::ConfigUnreadable);
    assert_eq!(ConfigError::NoHome.key(), ErrorKey::SettingsRefused);
    let write = ConfigError::Write(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    assert_eq!(write.key(), ErrorKey::SettingsRefused);
}
