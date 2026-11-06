//! The installer's start-up and loss rules, on literal views and loads.

use kumochi_protocol::message::{ErrorKey, PartState, VoiceView};
use kumochi_protocol::voice::VoicePart::{SpeechInput, SpokenReplies};

use super::{Durable, Installer, Load};
use crate::voice::{LoadEvent, LoadReport};

fn report(ask: u64, event: LoadEvent) -> LoadReport {
    LoadReport { ask, event }
}

fn view(speech_input: PartState, spoken_replies: PartState) -> VoiceView {
    VoiceView {
        speech_input,
        spoken_replies,
    }
}

fn failed(key: ErrorKey) -> PartState {
    PartState::Failed {
        key,
        installed: true,
    }
}

fn failed_absent(key: ErrorKey) -> PartState {
    PartState::Failed {
        key,
        installed: false,
    }
}

/// Both parts installed, whole and with their handles; each is loading.
fn both_loading() -> Installer {
    let (installer, loads) = Installer::start(
        [Durable::Installed, Durable::Installed],
        [true, true],
        [true, true],
    );
    assert_eq!(
        loads,
        [
            Load {
                part: SpeechInput,
                ask: 1
            },
            Load {
                part: SpokenReplies,
                ask: 2
            }
        ]
    );
    installer
}

#[test]
fn start_up_loads_each_whole_installed_part_and_nothing_else() {
    let (installer, loads) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true, false],
        [true, true],
    );
    assert_eq!(
        loads,
        [Load {
            part: SpeechInput,
            ask: 1
        }]
    );
    assert_eq!(
        installer.view(),
        view(PartState::Loading, PartState::NotInstalled)
    );
    assert!(!installer.ready(SpeechInput));
}

#[test]
fn a_part_with_a_short_file_is_corrupt_and_never_loads() {
    let (mut installer, loads) = Installer::start(
        [Durable::Installed, Durable::Installed],
        [false, true],
        [true, true],
    );
    assert_eq!(
        loads,
        [Load {
            part: SpokenReplies,
            ask: 2
        }]
    );
    assert_eq!(
        installer.view(),
        view(failed(ErrorKey::VoiceCorrupt), PartState::Loading)
    );
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        None
    );
    assert_eq!(
        installer.view().speech_input,
        failed(ErrorKey::VoiceCorrupt)
    );
}

#[test]
fn a_handle_that_didnt_start_is_a_load_failure_for_the_session() {
    let (mut installer, loads) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true, true],
        [false, false],
    );
    assert_eq!(loads, []);
    let fixed = view(
        failed(ErrorKey::VoiceLoad),
        failed_absent(ErrorKey::VoiceLoad),
    );
    assert_eq!(installer.view(), fixed);
    for part in [SpeechInput, SpokenReplies] {
        for ask in 1..=2 {
            assert_eq!(
                installer.reported(part, report(ask, LoadEvent::Loaded)),
                None
            );
        }
    }
    assert_eq!(installer.view(), fixed, "never Loading or Ready");
}

#[test]
fn a_removal_whose_delete_failed_shows_a_storage_failure_and_never_loads() {
    let (installer, loads) = Installer::start(
        [Durable::Removing, Durable::Absent],
        [true, true],
        [true, true],
    );
    assert_eq!(loads, []);
    assert_eq!(
        installer.view(),
        view(
            failed_absent(ErrorKey::VoiceStorage),
            PartState::NotInstalled
        )
    );
}

#[test]
fn a_loaded_part_is_ready_and_the_other_part_waits_for_its_own_report() {
    let mut installer = both_loading();
    assert_eq!(
        installer.reported(SpokenReplies, report(2, LoadEvent::Loaded)),
        None
    );
    assert!(installer.ready(SpokenReplies));
    assert!(!installer.ready(SpeechInput));
    assert_eq!(installer.view(), view(PartState::Loading, PartState::Ready));
}

#[test]
fn a_failed_load_is_a_load_failure_of_an_installed_part() {
    for event in [LoadEvent::Failed, LoadEvent::Discarded, LoadEvent::Unloaded] {
        let mut installer = both_loading();
        assert_eq!(installer.reported(SpeechInput, report(1, event)), None);
        assert_eq!(
            installer.view(),
            view(failed(ErrorKey::VoiceLoad), PartState::Loading),
            "{event:?}"
        );
    }
}

#[test]
fn a_loss_reloads_once_and_a_second_loss_is_a_load_failure() {
    let mut installer = both_loading();
    installer.reported(SpeechInput, report(1, LoadEvent::Loaded));
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Lost)),
        Some(Load {
            part: SpeechInput,
            ask: 3
        })
    );
    assert_eq!(installer.view().speech_input, PartState::Loading);
    installer.reported(SpeechInput, report(3, LoadEvent::Loaded));
    assert!(installer.ready(SpeechInput));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Lost)),
        None
    );
    assert_eq!(
        installer.view(),
        view(failed(ErrorKey::VoiceLoad), PartState::Loading)
    );
}

#[test]
fn a_loss_while_loading_reloads_too_and_each_part_has_its_own_allowance() {
    let mut installer = both_loading();
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Lost)),
        Some(Load {
            part: SpeechInput,
            ask: 3
        })
    );
    assert_eq!(
        installer.reported(SpokenReplies, report(2, LoadEvent::Lost)),
        Some(Load {
            part: SpokenReplies,
            ask: 4
        })
    );
    assert_eq!(
        installer.view(),
        view(PartState::Loading, PartState::Loading)
    );
}

#[test]
fn an_older_asks_reports_after_a_reload_change_nothing() {
    let mut installer = both_loading();
    installer.reported(SpeechInput, report(1, LoadEvent::Lost));
    installer.reported(SpeechInput, report(3, LoadEvent::Loaded));
    for event in [LoadEvent::Lost, LoadEvent::Loaded, LoadEvent::Failed] {
        assert_eq!(installer.reported(SpeechInput, report(1, event)), None);
    }
    assert!(installer.ready(SpeechInput), "the reload stays ready");
}
