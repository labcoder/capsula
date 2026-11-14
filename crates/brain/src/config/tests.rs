use std::path::PathBuf;

use kumochi_protocol::constants::{CONFIG_FILE, DEFAULT_ASSISTANT_NAME};
use kumochi_protocol::message::{
    AccountView, AssistantName, DockPosition, Effort, ErrorKey, MAX_SLUG_BYTES, MaxFrameRate,
    ModelChoice, ModelSlug, OnboardingStep, PlanState, RepoName, RunId, SettingsOp, SttUnload,
    ThreadId, UserName,
};
use kumochi_protocol::voice::VoicePart;

use super::{Config, ConfigError};
use crate::connections::github::device::DefaultRepo;
use crate::test_support::Scratch;

fn dir(scratch: &Scratch) -> PathBuf {
    let dir = scratch.db().with_file_name("config");
    std::fs::create_dir_all(&dir).expect("config folder");
    dir
}

fn name(text: &str) -> AssistantName {
    AssistantName::new(text).expect("valid")
}

fn saved(path: &std::path::Path) -> toml::Table {
    std::fs::read_to_string(path)
        .expect("readable")
        .parse()
        .expect("parses")
}

fn github_default(config: &Config) -> Option<(u64, RepoName)> {
    config
        .github_default()
        .map(|default| (default.account, default.repo))
}

#[test]
fn github_defaults_publish_only_on_commit_and_keep_unknown_keys_across_restart() {
    let scratch = Scratch::new("config-github-default");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(
        &path,
        "future_key = 3\n[later]\nmodel = \"x\"\n[github]\nfuture = [\"keep\"]\n\
         [github.options]\nenabled = true\n",
    )
    .expect("seeded");
    let original = saved(&path);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(github_default(&config), None);
    for (account, decimal, repo) in [
        (1, "1", "Owner/Repo"),
        (u64::MAX, "18446744073709551615", "Other/Next"),
    ] {
        let default = DefaultRepo {
            account,
            repo: RepoName::new(repo).expect("valid repo"),
        };
        let before = github_default(&config);
        let view = config.view().clone();
        let text = std::fs::read_to_string(&path).expect("readable");
        let pending = config.stage_github_default(&default).expect("staged");
        assert_eq!(github_default(&config), before);
        assert_eq!(config.view(), &view);
        assert_eq!(std::fs::read_to_string(&path).expect("readable"), text);
        pending.write().expect("written");
        assert_eq!(github_default(&config), before);
        assert_eq!(config.view(), &view);
        let table = saved(&path);
        assert_eq!(table["github"]["default_account"].as_str(), Some(decimal));
        assert_eq!(table["github"]["default_repo"].as_str(), Some(repo));
        assert_eq!(table["future_key"], original["future_key"]);
        assert_eq!(table["later"], original["later"]);
        assert_eq!(table["github"]["future"], original["github"]["future"]);
        assert_eq!(table["github"]["options"], original["github"]["options"]);
        config.commit(pending);
        assert_eq!(
            github_default(&config),
            Some((account, default.repo.clone()))
        );
        assert_eq!(config.view(), &view);
        let reloaded = Config::load(Some(dir.clone()), ThreadId(1));
        assert_eq!(github_default(&reloaded), Some((account, default.repo)));
    }
    let github = saved(&path)["github"].clone();
    apply(&mut config, SettingsOp::FinishOnboarding);
    assert_eq!(saved(&path)["github"], github);
    let reloaded = Config::load(Some(dir), ThreadId(1));
    assert_eq!(
        github_default(&reloaded),
        Some((u64::MAX, RepoName::new("Other/Next").expect("valid repo")))
    );
}

#[test]
fn malformed_or_incomplete_github_defaults_are_absent_and_survive_unrelated_saves() {
    let bad_accounts = [
        "\"\"",
        "\"0\"",
        "\"-1\"",
        "\"+1\"",
        "\" 1\"",
        "\"1 \"",
        "\"1.0\"",
        "\"1x\"",
        "\"18446744073709551616\"",
        "\"１２\"",
        "1",
        "0",
        "-1",
        "1.5",
        "true",
        "[]",
        "{}",
    ]
    .map(|account| {
        format!("[github]\ndefault_account = {account}\ndefault_repo = \"owner/repo\"\n")
    });
    let bad_repos = [
        "\"\"",
        "\"owner\"",
        "\"owner/\"",
        "\"/repo\"",
        "\"owner/repo/extra\"",
        "\"owner/re po\"",
        "1",
        "true",
        "[]",
        "{}",
    ]
    .map(|repo| format!("[github]\ndefault_account = \"1\"\ndefault_repo = {repo}\n"));
    let bad_sections = [
        "",
        "github = 3\n",
        "github = \"owner/repo\"\n",
        "github = []\n",
        "[github]\n",
        "[github]\ndefault_account = \"1\"\n",
        "[github]\ndefault_repo = \"owner/repo\"\n",
    ]
    .map(str::to_owned);
    let scratch = Scratch::new("config-github-malformed");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    for section in bad_accounts
        .into_iter()
        .chain(bad_repos)
        .chain(bad_sections)
    {
        let text = format!("future_key = 7\n{section}");
        std::fs::write(&path, &text).expect("seeded");
        let original = saved(&path);
        let mut config = Config::load(Some(dir.clone()), ThreadId(1));
        assert_eq!(github_default(&config), None, "{text}");
        apply(&mut config, SettingsOp::SetTaskbar(true));
        let table = saved(&path);
        assert_eq!(table.get("github"), original.get("github"), "{text}");
        assert_eq!(table["future_key"].as_integer(), Some(7), "{text}");
        let reloaded = Config::load(Some(dir.clone()), ThreadId(1));
        assert_eq!(github_default(&reloaded), None, "{text}");
    }
}

#[test]
fn setting_a_github_default_replaces_a_malformed_section_without_losing_other_keys() {
    let scratch = Scratch::new("config-github-repair");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, "future_key = 7\ngithub = false\n").expect("seeded");
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    let default = DefaultRepo {
        account: 17,
        repo: RepoName::new("owner/repo").expect("valid repo"),
    };
    let pending = config.stage_github_default(&default).expect("staged");
    pending.write().expect("written");
    config.commit(pending);
    assert_eq!(saved(&path)["future_key"].as_integer(), Some(7));
    let reloaded = Config::load(Some(dir), ThreadId(1));
    assert_eq!(github_default(&reloaded), Some((17, default.repo)));
}

#[test]
fn refused_github_defaults_preserve_the_file_view_and_default_in_later_saves() {
    let scratch = Scratch::new("config-github-refused");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let temp = dir.join(format!("{CONFIG_FILE}.tmp"));
    let text = "assistant_name = \"Nimbus\"\n[github]\n\
                default_account = \"7\"\ndefault_repo = \"Old/Repo\"\n";
    std::fs::write(&path, text).expect("seeded");
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    let before = Some((7, RepoName::new("Old/Repo").expect("valid repo")));
    assert_eq!(github_default(&config), before);
    let view = config.view().clone();
    let mut default = DefaultRepo {
        account: 0,
        repo: RepoName::new("New/Repo").expect("valid repo"),
    };
    assert!(matches!(
        config.stage_github_default(&default),
        Err(ConfigError::InvalidGitHubAccount)
    ));
    assert_eq!(
        ConfigError::InvalidGitHubAccount.key(),
        ErrorKey::SettingsRefused
    );
    assert_eq!(github_default(&config), before);
    assert_eq!(config.view(), &view);
    assert_eq!(std::fs::read_to_string(&path).expect("readable"), text);
    default.account = 8;
    std::fs::create_dir(&temp).expect("blocker");
    let pending = config.stage_github_default(&default).expect("staged");
    assert_eq!(github_default(&config), before);
    assert_eq!(config.view(), &view);
    assert!(matches!(pending.write(), Err(ConfigError::Write(_))));
    drop(pending);
    assert_eq!(github_default(&config), before);
    assert_eq!(config.view(), &view);
    assert_eq!(std::fs::read_to_string(&path).expect("readable"), text);
    std::fs::remove_dir(&temp).expect("unblocked");
    apply(&mut config, SettingsOp::FinishOnboarding);
    assert_eq!(github_default(&config), before);
    let reloaded = Config::load(Some(dir), ThreadId(1));
    assert_eq!(github_default(&reloaded), before);
}

#[test]
fn github_default_staging_honors_an_unavailable_or_unreadable_config() {
    let default = DefaultRepo {
        account: 1,
        repo: RepoName::new("owner/repo").expect("valid repo"),
    };
    let config = Config::load(None, ThreadId(1));
    assert_eq!(github_default(&config), None);
    assert!(matches!(
        config.stage_github_default(&default),
        Err(ConfigError::NoHome)
    ));
    let scratch = Scratch::new("config-github-unreadable");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let broken = "[github\n";
    std::fs::write(&path, broken).expect("seeded");
    let config = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(github_default(&config), None);
    assert!(matches!(
        config.stage_github_default(&default),
        Err(ConfigError::Unreadable)
    ));
    assert_eq!(std::fs::read_to_string(&path).expect("readable"), broken);
    std::fs::remove_file(&path).expect("removed broken fixture");
    std::fs::create_dir(&path).expect("a folder where the file goes");
    let config = Config::load(Some(dir), ThreadId(1));
    assert_eq!(github_default(&config), None);
    assert!(matches!(
        config.stage_github_default(&default),
        Err(ConfigError::Unreadable)
    ));
}

#[test]
fn hold_to_talk_toggle_has_its_own_wire_operation_and_persists() {
    let scratch = Scratch::new("config-hold-to-talk");
    let dir = dir(&scratch);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert!(config.view().hold_to_talk);
    for on in [false, true] {
        let op: SettingsOp = kumochi_protocol::frame::decode(&[12, u8::from(on)]).unwrap();
        apply(&mut config, op);
        assert_eq!(
            saved(&dir.join(CONFIG_FILE))["voice"]["hold_to_talk"].as_bool(),
            Some(on)
        );
        apply(&mut config, SettingsOp::SetSttUnload(SttUnload::After15Min));
        assert_eq!(
            Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .hold_to_talk,
            on
        );
    }
    for value in ["\"false\"", "1", "[]"] {
        let seeded = format!("[voice]\nhold_to_talk = {value}\nfuture = 7\n");
        std::fs::write(dir.join(CONFIG_FILE), &seeded).unwrap();
        assert!(
            !Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .hold_to_talk
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(),
            seeded
        );
    }
}

#[test]
fn screen_holds_default_on_persist_and_a_bad_value_reads_as_off() {
    let scratch = Scratch::new("config-screen-holds");
    let dir = dir(&scratch);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert!(config.view().screen_holds);
    for on in [false, true] {
        let op: SettingsOp = kumochi_protocol::frame::decode(&[14, u8::from(on)]).unwrap();
        apply(&mut config, op);
        assert_eq!(
            saved(&dir.join(CONFIG_FILE))["voice"]["screen_holds"].as_bool(),
            Some(on)
        );
        apply(&mut config, SettingsOp::SetHoldToTalk(true));
        assert_eq!(
            Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .screen_holds,
            on
        );
    }
    for value in ["\"true\"", "1", "[]"] {
        let seeded = format!("[voice]\nscreen_holds = {value}\n");
        std::fs::write(dir.join(CONFIG_FILE), &seeded).unwrap();
        assert!(
            !Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .screen_holds
        );
    }
}

#[test]
fn screen_holds_read_as_off_when_the_file_cant_be_read() {
    let scratch = Scratch::new("config-screen-holds-unread");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, "[voice]\nscreen_holds = true\nnot toml [").expect("seeded");
    let unparseable = Config::load(Some(dir.clone()), ThreadId(1));
    std::fs::remove_file(&path).expect("removed broken fixture");
    std::fs::create_dir(&path).expect("a folder where the file goes");
    let unreadable = Config::load(Some(dir), ThreadId(1));
    for (cause, config) in [
        ("unparseable", unparseable),
        ("unreadable", unreadable),
        ("no home", Config::load(None, ThreadId(1))),
    ] {
        assert!(!config.view().screen_holds, "{cause}");
    }
}

#[test]
fn talk_back_and_the_unload_policy_default_off_and_kept_and_are_saved_in_the_voice_table() {
    let scratch = Scratch::new("config-voice");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert!(!config.view().talk_back);
    assert_eq!(config.view().stt_unload, SttUnload::Kept);
    for (unload, name) in [
        (SttUnload::After15Min, "15m"),
        (SttUnload::After1Hour, "1h"),
        (SttUnload::Kept, "kept"),
    ] {
        for on in [true, false] {
            apply(&mut config, SettingsOp::SetTalkBack(on));
            apply(&mut config, SettingsOp::SetSttUnload(unload));
            let saved = saved(&path);
            assert_eq!(saved["voice"]["talk_back"].as_bool(), Some(on));
            assert_eq!(saved["voice"]["stt_unload"].as_str(), Some(name));
            let reloaded = Config::load(Some(dir.clone()), ThreadId(1));
            assert_eq!(reloaded.view().talk_back, on);
            assert_eq!(reloaded.view().stt_unload, unload);
        }
    }
}

#[test]
fn bad_voice_values_read_as_the_defaults_and_outlive_writes_to_other_keys() {
    let scratch = Scratch::new("config-voice-bad");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(
        &path,
        "[voice]\ntalk_back = \"yes\"\nstt_unload = \"2h\"\n\
         installed = [\"speech_input\", \"later_part\"]\nremoving = [\"spoken_replies\"]\n\
         future = 1\n",
    )
    .expect("seeded");
    let original = saved(&path);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert!(!config.view().talk_back);
    assert_eq!(config.view().stt_unload, SttUnload::Kept);
    apply(&mut config, SettingsOp::SetTaskbar(true));
    assert_eq!(saved(&path)["voice"], original["voice"]);
    apply(&mut config, SettingsOp::SetTalkBack(true));
    let after = saved(&path);
    assert_eq!(after["voice"]["talk_back"].as_bool(), Some(true));
    for key in ["stt_unload", "installed", "removing", "future"] {
        assert_eq!(after["voice"][key], original["voice"][key], "{key}");
    }
    apply(&mut config, SettingsOp::SetSttUnload(SttUnload::After1Hour));
    let after = saved(&path);
    assert_eq!(after["voice"]["stt_unload"].as_str(), Some("1h"));
    assert_eq!(after["voice"]["talk_back"].as_bool(), Some(true));
    for key in ["installed", "removing", "future"] {
        assert_eq!(after["voice"][key], original["voice"][key], "{key}");
    }
    let reloaded = Config::load(Some(dir), ThreadId(1));
    assert!(reloaded.view().talk_back);
    assert_eq!(reloaded.view().stt_unload, SttUnload::After1Hour);
}

#[test]
fn installed_names_known_parts_and_a_removal_wins() {
    for (voice, expected) in [
        (
            "installed = [\"spoken-replies\", \"later-part\", \"speech-input\"]\nremoving = [\"speech-input\"]",
            vec![VoicePart::SpokenReplies],
        ),
        (
            "installed = [\"spoken-replies\", \"speech-input\"]",
            vec![VoicePart::SpeechInput, VoicePart::SpokenReplies],
        ),
        ("removing = [\"speech-input\"]", vec![]),
        ("installed = \"speech-input\"", vec![]),
    ] {
        let scratch = Scratch::new("config-installed");
        let dir = dir(&scratch);
        std::fs::write(dir.join(CONFIG_FILE), format!("[voice]\n{voice}\n")).expect("seeded");
        let config = Config::load(Some(dir), ThreadId(1));
        assert_eq!(config.installed(), expected, "{voice}");
    }
}

#[test]
fn removing_names_known_parts_by_their_folder_names_only() {
    for (list, expected) in [
        (
            "[\"later-part\", \"spoken-replies\", \"spoken_replies\", \"spoken-replies\"]",
            vec![VoicePart::SpokenReplies],
        ),
        (
            "[\"spoken-replies\", \"speech-input\"]",
            vec![VoicePart::SpeechInput, VoicePart::SpokenReplies],
        ),
        ("[]", vec![]),
        ("\"speech-input\"", vec![]),
        ("[1, \"Speech-Input\"]", vec![]),
    ] {
        let scratch = Scratch::new("config-removing");
        let dir = dir(&scratch);
        std::fs::write(
            dir.join(CONFIG_FILE),
            format!("[voice]\ninstalled = [\"speech-input\"]\nremoving = {list}\n"),
        )
        .expect("seeded");
        let config = Config::load(Some(dir), ThreadId(1));
        assert_eq!(config.removing(), expected, "{list}");
    }
    let scratch = Scratch::new("config-removing-none");
    assert_eq!(
        Config::load(Some(dir(&scratch)), ThreadId(1)).removing(),
        []
    );
}

#[test]
fn a_finished_removal_takes_only_its_part_out_of_removing() {
    let scratch = Scratch::new("config-remove-done");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(
        &path,
        "[voice]\ntalk_back = true\ninstalled = [\"spoken-replies\"]\n\
         removing = [\"speech-input\", \"later-part\", \"spoken-replies\", \"speech-input\"]\n",
    )
    .expect("seeded");
    let mut config = Config::load(Some(dir), ThreadId(1));
    let view = config.view().clone();
    let pending = config
        .stage_remove_done(VoicePart::SpeechInput)
        .expect("staged");
    pending.write().expect("written");
    config.commit(pending);
    let voice = &saved(&path)["voice"];
    assert_eq!(
        voice["removing"],
        toml::Value::Array(vec!["later-part".into(), "spoken-replies".into()])
    );
    assert_eq!(
        voice["installed"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
    assert_eq!(voice["talk_back"].as_bool(), Some(true));
    assert_eq!(config.view(), &view);
    assert_eq!(config.removing(), [VoicePart::SpokenReplies]);
}

#[test]
fn an_install_joins_installed_once_and_turns_talk_back_on_only_if_the_user_never_chose() {
    for (part, voice, installed, talk_back) in [
        (
            VoicePart::SpokenReplies,
            "installed = [\"later-part\"]",
            vec!["later-part", "spoken-replies"],
            Some(true),
        ),
        (
            VoicePart::SpokenReplies,
            "talk_back = false",
            vec!["spoken-replies"],
            Some(false),
        ),
        (VoicePart::SpeechInput, "", vec!["speech-input"], None),
        (
            VoicePart::SpeechInput,
            "installed = [\"speech-input\"]",
            vec!["speech-input"],
            None,
        ),
    ] {
        let scratch = Scratch::new("config-voice-activate");
        let dir = dir(&scratch);
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, format!("[voice]\n{voice}\n")).expect("seeded");
        let mut config = Config::load(Some(dir), ThreadId(1));
        let pending = config.stage_activate(part).expect("staged");
        pending.write().expect("written");
        config.commit(pending);
        let saved = &saved(&path)["voice"];
        assert_eq!(
            saved["installed"],
            toml::Value::Array(installed.into_iter().map(Into::into).collect()),
            "{voice:?}"
        );
        assert_eq!(
            saved.get("talk_back").and_then(toml::Value::as_bool),
            talk_back,
            "{voice:?}"
        );
        assert_eq!(
            config.view().talk_back,
            talk_back == Some(true),
            "{voice:?}"
        );
    }
}

#[test]
fn only_the_three_exact_unload_names_are_read() {
    for bad in ["", "15M", " 1h", "1h ", "Kept", "2h"] {
        let scratch = Scratch::new("config-voice-names");
        let dir = dir(&scratch);
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, format!("[voice]\nstt_unload = {bad:?}\n")).expect("seeded");
        let mut config = Config::load(Some(dir), ThreadId(1));
        assert_eq!(config.view().stt_unload, SttUnload::Kept, "{bad:?}");
        apply(&mut config, SettingsOp::SetTaskbar(true));
        assert_eq!(saved(&path)["voice"]["stt_unload"].as_str(), Some(bad));
    }
}

#[test]
fn an_empty_voice_table_reads_as_the_defaults_and_takes_a_setting() {
    let scratch = Scratch::new("config-voice-empty");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, "[voice]\n").expect("seeded");
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert!(!config.view().talk_back);
    assert_eq!(config.view().stt_unload, SttUnload::Kept);
    apply(&mut config, SettingsOp::FinishOnboarding);
    assert_eq!(
        saved(&path)["voice"].as_table().map(toml::Table::len),
        Some(0)
    );
    apply(&mut config, SettingsOp::SetSttUnload(SttUnload::After15Min));
    assert_eq!(saved(&path)["voice"]["stt_unload"].as_str(), Some("15m"));
    assert_eq!(
        Config::load(Some(dir), ThreadId(1)).view().stt_unload,
        SttUnload::After15Min
    );
}

#[test]
fn a_voice_key_that_isnt_a_table_reads_as_the_defaults_until_a_voice_setting_replaces_it() {
    for seeded in ["voice = 3\n", "[voice]\ntalk_back = 1\nstt_unload = 15\n"] {
        let scratch = Scratch::new("config-voice-shape");
        let dir = dir(&scratch);
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, seeded).expect("seeded");
        let original = saved(&path);
        let mut config = Config::load(Some(dir.clone()), ThreadId(1));
        assert!(!config.view().talk_back, "{seeded}");
        assert_eq!(config.view().stt_unload, SttUnload::Kept, "{seeded}");
        apply(&mut config, SettingsOp::FinishOnboarding);
        assert_eq!(saved(&path)["voice"], original["voice"], "{seeded}");
        apply(&mut config, SettingsOp::SetTalkBack(true));
        assert_eq!(saved(&path)["voice"]["talk_back"].as_bool(), Some(true));
        assert!(Config::load(Some(dir), ThreadId(1)).view().talk_back);
    }
}

#[test]
fn the_installer_operations_are_refused_and_change_nothing() {
    let scratch = Scratch::new("config-voice-dormant");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir), ThreadId(1));
    apply(&mut config, SettingsOp::SetTalkBack(true));
    let before = std::fs::read_to_string(&path).expect("readable");
    let view = config.view().clone();
    for op in [
        SettingsOp::InstallVoice(VoicePart::SpeechInput),
        SettingsOp::InstallVoice(VoicePart::SpokenReplies),
        SettingsOp::RemoveVoice(VoicePart::SpeechInput),
        SettingsOp::RemoveVoice(VoicePart::SpokenReplies),
        SettingsOp::CancelVoiceInstall,
    ] {
        let refused = config.stage(op).err();
        assert!(matches!(refused, Some(ConfigError::VoiceUnavailable)));
        assert_eq!(
            refused.map(|error| error.key()),
            Some(ErrorKey::VoiceUnavailable)
        );
        assert_eq!(config.view(), &view);
        assert_eq!(std::fs::read_to_string(&path).expect("readable"), before);
    }
}

fn apply(config: &mut Config, op: SettingsOp) {
    let before = config.view().clone();
    let pending = config.stage(op).expect("staged");
    assert_eq!(config.view(), &before);
    pending.write().expect("written");
    assert_eq!(config.view(), &before);
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
    assert_eq!(view.dock_position, DockPosition::TopCenter);
    assert!(!view.show_in_taskbar);
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
fn the_max_frame_rate_persists_and_automatic_removes_the_key() {
    let scratch = Scratch::new("config-fps");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(config.view().max_frame_rate, MaxFrameRate::AUTOMATIC);
    let cap = MaxFrameRate::cap(144).expect("a cap");
    apply(&mut config, SettingsOp::SetMaxFrameRate(cap));
    let reloaded = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(reloaded.view().max_frame_rate.fps(), Some(144));
    apply(
        &mut config,
        SettingsOp::SetMaxFrameRate(MaxFrameRate::AUTOMATIC),
    );
    let text = std::fs::read_to_string(&path).expect("readable");
    assert!(!text.contains("max_frame_rate"), "{text}");
    for bad in ["61", "0", "70000", "-30", "\"60\""] {
        std::fs::write(&path, format!("max_frame_rate = {bad}\n")).expect("seeded");
        let config = Config::load(Some(dir.clone()), ThreadId(1));
        assert_eq!(
            config.view().max_frame_rate,
            MaxFrameRate::AUTOMATIC,
            "{bad}"
        );
    }
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

#[cfg(windows)]
#[test]
fn a_held_reader_refuses_atomic_config_replacement_without_changing_settings() {
    let scratch = Scratch::new("config-open-reader");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir), ThreadId(1));
    apply(&mut config, SettingsOp::SetAssistantName(name("Nimbus")));
    let before = std::fs::read(&path).expect("readable");
    let reader = std::fs::File::open(&path).expect("held reader");
    let pending = config
        .stage(SettingsOp::SetAssistantName(name("Cirrus")))
        .expect("staged");
    let Err(ConfigError::Write(error)) = pending.write() else {
        panic!("a held reader must refuse replacement");
    };
    assert_eq!(error.raw_os_error(), Some(5));
    assert_eq!(config.view().assistant_name.as_str(), "Nimbus");
    assert_eq!(std::fs::read(&path).expect("old file readable"), before);
    drop(reader);
    pending
        .write()
        .expect("replacement after the reader closes");
    config.commit(pending);
    assert_eq!(config.view().assistant_name.as_str(), "Cirrus");
    assert_eq!(saved(&path)["assistant_name"].as_str(), Some("Cirrus"));
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

#[test]
fn every_dock_position_and_taskbar_choice_persists() {
    let scratch = Scratch::new("config-dock");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    let cases = [
        ("top_left", DockPosition::TopLeft),
        ("top_center", DockPosition::TopCenter),
        ("top_right", DockPosition::TopRight),
        ("middle_left", DockPosition::MiddleLeft),
        ("center", DockPosition::Center),
        ("middle_right", DockPosition::MiddleRight),
        ("bottom_left", DockPosition::BottomLeft),
        ("bottom_center", DockPosition::BottomCenter),
        ("bottom_right", DockPosition::BottomRight),
    ];
    for (literal, position) in cases {
        std::fs::write(&path, format!("dock_position = \"{literal}\"\n")).expect("seeded");
        assert_eq!(
            Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .dock_position,
            position,
            "{literal}"
        );
        apply(&mut config, SettingsOp::SetDockPosition(position));
        let table: toml::Table = std::fs::read_to_string(&path)
            .expect("readable")
            .parse()
            .expect("parses");
        assert_eq!(table["dock_position"].as_str(), Some(literal));
        assert_eq!(config.view().dock_position, position);
        assert_eq!(
            Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .dock_position,
            position
        );
    }
    for shown in [true, false] {
        apply(&mut config, SettingsOp::SetTaskbar(shown));
        assert_eq!(config.view().show_in_taskbar, shown);
        assert_eq!(
            Config::load(Some(dir.clone()), ThreadId(1))
                .view()
                .show_in_taskbar,
            shown
        );
        let table: toml::Table = std::fs::read_to_string(&path)
            .expect("readable")
            .parse()
            .expect("parses");
        assert_eq!(table["show_in_taskbar"].as_bool(), Some(shown));
    }
}

#[test]
fn invalid_dock_and_taskbar_values_survive_unrelated_saves_until_explicitly_set() {
    let scratch = Scratch::new("config-invalid-dock");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    for dock in ["\"unknown\"", "\"\"", "5", "true", "{}"] {
        for taskbar in ["\"yes\"", "1"] {
            let text =
                format!("dock_position = {dock}\nshow_in_taskbar = {taskbar}\nfuture_key = 7\n");
            std::fs::write(&path, &text).expect("seeded");
            let original: toml::Table = text.parse().expect("parses");
            let mut config = Config::load(Some(dir.clone()), ThreadId(1));
            assert_eq!(config.view().dock_position, DockPosition::TopCenter);
            assert!(!config.view().show_in_taskbar);
            apply(&mut config, SettingsOp::FinishOnboarding);
            let saved: toml::Table = std::fs::read_to_string(&path)
                .expect("readable")
                .parse()
                .expect("parses");
            assert_eq!(saved["dock_position"], original["dock_position"]);
            assert_eq!(saved["show_in_taskbar"], original["show_in_taskbar"]);
            assert_eq!(saved["future_key"].as_integer(), Some(7));
            let reloaded = Config::load(Some(dir.clone()), ThreadId(1));
            assert_eq!(reloaded.view().dock_position, DockPosition::TopCenter);
            assert!(!reloaded.view().show_in_taskbar);
            apply(
                &mut config,
                SettingsOp::SetDockPosition(DockPosition::TopCenter),
            );
            apply(&mut config, SettingsOp::SetTaskbar(false));
            let saved: toml::Table = std::fs::read_to_string(&path)
                .expect("readable")
                .parse()
                .expect("parses");
            assert_eq!(saved["dock_position"].as_str(), Some("top_center"));
            assert_eq!(saved["show_in_taskbar"].as_bool(), Some(false));
        }
    }
}

#[test]
fn refused_dock_and_taskbar_writes_do_not_leak_into_a_later_save() {
    let scratch = Scratch::new("config-refused-dock");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let temp = dir.join(format!("{CONFIG_FILE}.tmp"));
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    apply(
        &mut config,
        SettingsOp::SetDockPosition(DockPosition::MiddleLeft),
    );
    apply(&mut config, SettingsOp::SetTaskbar(false));
    let before = std::fs::read_to_string(&path).expect("readable");
    let view = config.view().clone();
    std::fs::create_dir(&temp).expect("blocker");
    for op in [
        SettingsOp::SetDockPosition(DockPosition::BottomRight),
        SettingsOp::SetTaskbar(true),
    ] {
        let pending = config.stage(op).expect("staged");
        assert_eq!(config.view(), &view);
        assert!(matches!(pending.write(), Err(ConfigError::Write(_))));
        drop(pending);
        assert_eq!(config.view(), &view);
        assert_eq!(std::fs::read_to_string(&path).expect("readable"), before);
    }
    std::fs::remove_dir(&temp).expect("unblocked");
    apply(&mut config, SettingsOp::FinishOnboarding);
    let reloaded = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(reloaded.view().dock_position, DockPosition::MiddleLeft);
    assert!(!reloaded.view().show_in_taskbar);
    assert!(reloaded.view().onboarded);
    apply(
        &mut config,
        SettingsOp::SetDockPosition(DockPosition::BottomCenter),
    );
    apply(&mut config, SettingsOp::SetTaskbar(true));
    let reloaded = Config::load(Some(dir), ThreadId(1));
    assert_eq!(reloaded.view().dock_position, DockPosition::BottomCenter);
    assert!(reloaded.view().show_in_taskbar);
}

fn choice(slug: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: ModelSlug::new(slug).expect("valid"),
        effort,
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
    }
}

#[test]
fn the_default_model_preserves_identity_when_only_effort_is_unknown() {
    let scratch = Scratch::new("config-default-model");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let mut config = Config::load(Some(dir.clone()), ThreadId(1));
    assert_eq!(config.view().default_model, None);
    let luna = choice("gpt-6-luna", Effort::Low);
    apply(&mut config, SettingsOp::SetDefaultModel(luna.clone()));
    assert_eq!(config.view().default_model, Some(luna.clone()));
    let table: toml::Table = std::fs::read_to_string(&path)
        .expect("readable")
        .parse()
        .expect("parses");
    assert_eq!(table["default_model"]["model"].as_str(), Some("gpt-6-luna"));
    assert_eq!(table["default_model"]["effort"].as_str(), Some("Low"));
    let reload = || {
        Config::load(Some(dir.clone()), ThreadId(1))
            .view()
            .default_model
            .clone()
    };
    assert_eq!(reload(), Some(luna));
    let longest = "a".repeat(MAX_SLUG_BYTES);
    let over = "a".repeat(MAX_SLUG_BYTES + 1);
    let cases = [
        (
            format!("default_model = {{ model = \"{longest}\", effort = \"Max\" }}"),
            Some(choice(&longest, Effort::Max)),
        ),
        (
            format!("default_model = {{ model = \"{over}\", effort = \"Max\" }}"),
            None,
        ),
        ("default_model = \"gpt-6-luna\"".to_owned(), None),
        (
            "default_model = { model = \"Not A Slug\", effort = \"Low\" }".to_owned(),
            None,
        ),
        (
            "default_model = { model = \"gpt-6-luna\", effort = \"ultra\" }".to_owned(),
            Some(choice("gpt-6-luna", Effort::Medium)),
        ),
        (
            "default_model = { model = \"gpt-6-luna\" }".to_owned(),
            None,
        ),
        ("default_model = {}".to_owned(), None),
    ];
    for (text, expected) in cases {
        std::fs::write(&path, &text).expect("seeded");
        assert_eq!(reload(), expected, "{text}");
    }
}

#[test]
fn claude_code_is_on_unless_the_file_could_have_turned_it_off() {
    let cases: [(&str, Option<&str>, bool); 7] = [
        ("no-file", None, true),
        ("no-section", Some("onboarded = true\n"), true),
        ("no-key", Some("[claude_code]\n"), true),
        ("on", Some("[claude_code]\nenabled = true\n"), true),
        ("off", Some("[claude_code]\nenabled = false\n"), false),
        ("mistyped", Some("[claude_code]\nenabled = \"no\"\n"), false),
        ("not-a-section", Some("claude_code = false\n"), false),
    ];
    for (name, text, enabled) in cases {
        let scratch = Scratch::new(&format!("config-claude-{name}"));
        let dir = dir(&scratch);
        if let Some(text) = text {
            std::fs::write(dir.join(CONFIG_FILE), text).expect("seeded");
        }
        let config = Config::load(Some(dir), ThreadId(1));
        assert_eq!(config.claude_code_enabled(), enabled, "{name}");
    }
    let scratch = Scratch::new("config-claude-unreadable");
    let dir = dir(&scratch);
    std::fs::write(dir.join(CONFIG_FILE), "[claude_code\n").expect("seeded");
    assert!(
        !Config::load(Some(dir), ThreadId(1)).claude_code_enabled(),
        "a file that can't be parsed may have turned it off"
    );
}

#[test]
fn a_removal_intent_moves_only_its_part_from_installed_to_removing() {
    let scratch = Scratch::new("config-remove-intent");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(
        &path,
        "[voice]\ntalk_back = false\n\
         installed = [\"speech-input\", \"later-part\", \"spoken-replies\", \"speech-input\"]\n\
         removing = [\"other-part\"]\n",
    )
    .expect("seeded");
    let mut config = Config::load(Some(dir), ThreadId(1));
    let view = config.view().clone();
    let pending = config
        .stage_remove_intent(VoicePart::SpeechInput)
        .expect("staged");
    pending.write().expect("written");
    config.commit(pending);
    let voice = &saved(&path)["voice"];
    assert_eq!(
        voice["installed"],
        toml::Value::Array(vec!["later-part".into(), "spoken-replies".into()])
    );
    assert_eq!(
        voice["removing"],
        toml::Value::Array(vec!["other-part".into(), "speech-input".into()])
    );
    assert_eq!(voice["talk_back"].as_bool(), Some(false));
    assert_eq!(config.view(), &view);
    assert_eq!(config.installed(), [VoicePart::SpokenReplies]);
    assert_eq!(config.removing(), [VoicePart::SpeechInput]);
}

#[test]
fn a_removal_intent_with_no_lists_starts_removing() {
    let scratch = Scratch::new("config-remove-intent-empty");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, "[voice]\ninstalled = 3\n").expect("seeded");
    let mut config = Config::load(Some(dir), ThreadId(1));
    let pending = config
        .stage_remove_intent(VoicePart::SpokenReplies)
        .expect("staged");
    pending.write().expect("written");
    config.commit(pending);
    let voice = &saved(&path)["voice"];
    assert_eq!(
        voice["installed"].as_integer(),
        Some(3),
        "not a list, so left alone"
    );
    assert_eq!(
        voice["removing"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
}

#[test]
fn an_absent_onboarded_key_counts_a_profile_in_use_and_a_saved_value_wins() {
    let scratch = Scratch::new("config-onboarded-in-use");
    let signed_in = |plan| AccountView::SignedIn {
        email: None,
        plan,
        recovery: None,
    };
    let signed_out = AccountView::SignedOut {
        revocation_unconfirmed: false,
    };
    let absent = "future_key = 1\n";
    for (row, (text, account, high_water, onboarded)) in [
        (absent, signed_out.clone(), 0, false),
        (absent, AccountView::Busy, 0, false),
        (absent, signed_in(PlanState::Enabled), 0, true),
        (absent, signed_in(PlanState::Declined), 0, true),
        (absent, signed_out.clone(), 1, true),
        (
            "onboarded = false\n",
            signed_in(PlanState::Enabled),
            5,
            false,
        ),
        ("onboarded = true\n", signed_out.clone(), 0, true),
        ("onboarded = \"yes\"\n", signed_out, 0, false),
        (
            "onboarded = \"yes\"\n",
            signed_in(PlanState::Enabled),
            0,
            true,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = dir(&scratch).join(row.to_string());
        std::fs::create_dir_all(&dir).expect("row folder");
        std::fs::write(dir.join(CONFIG_FILE), text).expect("seeded");
        let mut config = Config::load(Some(dir), ThreadId(1));
        config.count_in_use(&account, RunId(high_water));
        assert_eq!(config.view().onboarded, onboarded, "row {row}");
    }
}

#[test]
fn the_start_answer_outlives_other_writes_and_finishing_onboarding_is_saved() {
    let scratch = Scratch::new("config-onboarded-kept");
    let used = dir(&scratch);
    let path = used.join(CONFIG_FILE);
    std::fs::write(&path, "future_key = 1\n").expect("seeded");
    let mut config = Config::load(Some(used), ThreadId(1));
    let signed_out = AccountView::SignedOut {
        revocation_unconfirmed: false,
    };
    config.count_in_use(&signed_out, RunId(3));
    assert!(config.view().onboarded);
    apply(&mut config, SettingsOp::SetTaskbar(true));
    assert!(config.view().onboarded);
    assert!(config.view().show_in_taskbar);
    assert_eq!(saved(&path).get("onboarded"), None);

    let fresh = dir(&scratch).join("fresh");
    let mut config = Config::load(Some(fresh.clone()), ThreadId(1));
    config.count_in_use(&signed_out, RunId(0));
    assert!(!config.view().onboarded);
    apply(&mut config, SettingsOp::FinishOnboarding);
    assert!(config.view().onboarded);
    assert_eq!(
        saved(&fresh.join(CONFIG_FILE))["onboarded"].as_bool(),
        Some(true)
    );
    assert!(Config::load(Some(fresh), ThreadId(1)).view().onboarded);
}

#[test]
fn completed_onboarding_steps_only_move_forward_and_the_finish_clears_them() {
    let scratch = Scratch::new("config-onboarding-from");
    let fresh = dir(&scratch);
    let path = fresh.join(CONFIG_FILE);
    let mut config = Config::load(Some(fresh.clone()), ThreadId(1));
    assert_eq!(config.view().onboarding_from, OnboardingStep::Welcome);
    apply(
        &mut config,
        SettingsOp::CompleteOnboardingStep(OnboardingStep::YourName),
    );
    assert_eq!(config.view().onboarding_from, OnboardingStep::Accounts);
    for done in [OnboardingStep::Welcome, OnboardingStep::Shortcut] {
        apply(&mut config, SettingsOp::CompleteOnboardingStep(done));
        assert_eq!(
            config.view().onboarding_from,
            OnboardingStep::Accounts,
            "{done:?}"
        );
    }
    assert_eq!(saved(&path)["onboarding_from"].as_str(), Some("accounts"));
    assert_eq!(saved(&path).get("onboarded"), None);
    assert_eq!(
        Config::load(Some(fresh.clone()), ThreadId(1))
            .view()
            .onboarding_from,
        OnboardingStep::Accounts
    );
    apply(&mut config, SettingsOp::FinishOnboarding);
    assert_eq!(saved(&path).get("onboarding_from"), None);
    std::fs::write(&path, "onboarding_from = \"later\"\n").expect("seeded");
    assert_eq!(
        Config::load(Some(fresh), ThreadId(1))
            .view()
            .onboarding_from,
        OnboardingStep::Welcome
    );
}

#[test]
fn a_config_that_cant_be_written_serves_onboarded_and_still_refuses_every_write() {
    let scratch = Scratch::new("config-onboarded-refusing");
    let dir = dir(&scratch);
    let path = dir.join(CONFIG_FILE);
    let broken = "onboarded = false\nnot toml [";
    std::fs::write(&path, broken).expect("seeded");
    let default = DefaultRepo {
        account: 1,
        repo: RepoName::new("owner/repo").expect("valid repo"),
    };
    for (mut config, refusal) in [
        (
            Config::load(Some(dir), ThreadId(1)),
            ConfigError::Unreadable,
        ),
        (Config::load(None, ThreadId(1)), ConfigError::NoHome),
    ] {
        assert!(config.view().onboarded, "{refusal}");
        config.count_in_use(
            &AccountView::SignedOut {
                revocation_unconfirmed: false,
            },
            RunId(0),
        );
        assert!(config.view().onboarded, "{refusal}");
        let view = config.view().clone();
        let ops = [
            SettingsOp::SetAssistantName(name("Nimbus")),
            SettingsOp::SetUserName(Some(UserName::new("Oscar").expect("valid"))),
            SettingsOp::SetUserName(None),
            SettingsOp::FinishOnboarding,
            SettingsOp::SetMaxFrameRate(MaxFrameRate::AUTOMATIC),
            SettingsOp::SetDefaultModel(choice("gpt-6-luna", Effort::Low)),
            SettingsOp::SetDockPosition(DockPosition::Center),
            SettingsOp::SetTaskbar(true),
            SettingsOp::SetTalkBack(true),
            SettingsOp::SetSttUnload(SttUnload::After1Hour),
            SettingsOp::InstallVoice(VoicePart::SpeechInput),
            SettingsOp::RemoveVoice(VoicePart::SpokenReplies),
            SettingsOp::CancelVoiceInstall,
        ];
        let staged = ops.into_iter().map(|op| config.stage(op)).chain([
            config.stage_github_default(&default),
            config.stage_activate(VoicePart::SpeechInput),
            config.stage_remove_intent(VoicePart::SpeechInput),
            config.stage_remove_done(VoicePart::SpeechInput),
        ]);
        for (index, staged) in staged.enumerate() {
            let error = staged.expect_err("refused");
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&refusal),
                "{refusal} write {index}"
            );
        }
        assert_eq!(config.view(), &view);
    }
    assert_eq!(std::fs::read_to_string(&path).expect("readable"), broken);
}
