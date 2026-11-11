//! The installer's start-up, install, cancel and loss rules, on literal views and effects.

use kumochi_protocol::message::{ErrorKey, PartState, VoiceView};
use kumochi_protocol::voice::VoicePart::{self, SpeechInput, SpokenReplies};

use super::{Durable, Effect, Installer};
use crate::voice::download::{DownloadError, FetchPhase, Progress};
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
        &[],
    );
    assert_eq!(
        loads,
        [
            Effect::Load {
                part: SpeechInput,
                ask: 1
            },
            Effect::Load {
                part: SpokenReplies,
                ask: 2
            }
        ]
    );
    installer
}

#[test]
fn an_idle_unloaded_part_removes_without_an_extra_unload_or_reload() {
    use super::{Folder, RemoveAnswer};

    let mut installer = both_loading();
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::Unloaded);
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Intent { token: 3 }, vec![])
    );
    assert_eq!(
        installer.intended(SpeechInput, 3, Ok(())),
        [Effect::Delete {
            part: SpeechInput,
            token: 3
        }]
    );
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
}

#[test]
fn key_down_reloads_only_after_the_matching_idle_report_with_a_fresh_ask() {
    let mut installer = both_loading();
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.reported(SpokenReplies, report(2, LoadEvent::Loaded)),
        []
    );
    assert_eq!(installer.reload_input(), []);
    assert_eq!(
        installer.reported(SpeechInput, report(9, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(installer.reload_input(), []);
    assert_eq!(installer.view(), view(PartState::Ready, PartState::Ready));
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(
        installer.reload_input(),
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(installer.parts[0].durable, Durable::Installed);
    assert_eq!(installer.parts[0].models, [3]);
    assert_eq!(installer.view(), view(PartState::Loading, PartState::Ready));
    assert_eq!(installer.reload_input(), []);
    assert_eq!(installer.next_token, 4);
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        []
    );
    assert_eq!(installer.view(), view(PartState::Ready, PartState::Ready));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one state sequence checks reload suppression through removal and shutdown"
)]
fn ineligible_key_down_never_queues_an_install_or_replays_after_removal() {
    use super::{Folder, RemoveAnswer};

    fn dropped(installer: &mut Installer) {
        let before = installer.view();
        let token = installer.parts[0].token;
        let next = installer.next_token;
        assert_eq!(installer.reload_input(), []);
        assert_eq!(installer.view(), before);
        assert_eq!(installer.parts[0].token, token);
        assert_eq!(installer.next_token, next);
        assert!(!installer.parts[0].install_after);
    }

    for (durable, whole, available, expected) in [
        (Durable::Absent, true, true, PartState::NotInstalled),
        (
            Durable::Installed,
            false,
            true,
            failed(ErrorKey::VoiceCorrupt),
        ),
        (Durable::Installed, true, false, failed(ErrorKey::VoiceLoad)),
        (Durable::Installed, true, true, PartState::Loading),
    ] {
        let (mut installer, _) = Installer::start(
            [durable, Durable::Absent],
            [whole, true],
            [available, true],
            &[],
        );
        assert_eq!(installer.view(), view(expected, PartState::NotInstalled));
        dropped(&mut installer);
    }

    let mut cancelling = both_loading();
    assert_eq!(
        cancelling.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        cancelling.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(
        cancelling.reload_input(),
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(
        cancelling.cancel(),
        [Effect::Unload {
            part: SpeechInput,
            ask: 3
        }]
    );
    dropped(&mut cancelling);

    for committed in [false, true] {
        let mut installer = both_loading();
        assert_eq!(
            installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
            []
        );
        assert_eq!(
            installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
            []
        );
        assert_eq!(
            installer.remove(SpeechInput, Folder::Present),
            (RemoveAnswer::Intent { token: 3 }, vec![])
        );
        assert_eq!(installer.view().speech_input, PartState::Unloaded);
        dropped(&mut installer);
        if committed {
            assert_eq!(
                installer.intended(SpeechInput, 3, Ok(())),
                [Effect::Delete {
                    part: SpeechInput,
                    token: 3
                }]
            );
            dropped(&mut installer);
            assert_eq!(
                installer.deleted(SpeechInput, 3, Folder::Missing),
                [Effect::Clear { part: SpeechInput }]
            );
            dropped(&mut installer);
            assert_eq!(installer.cleared(SpeechInput, Ok(())), []);
            assert_eq!(installer.view().speech_input, PartState::NotInstalled);
            dropped(&mut installer);
        } else {
            assert_eq!(
                installer.intended(SpeechInput, 3, Err(ErrorKey::SettingsRefused)),
                []
            );
            assert_eq!(installer.view().speech_input, PartState::Unloaded);
            assert_eq!(
                installer.reload_input(),
                [Effect::Load {
                    part: SpeechInput,
                    ask: 4
                }]
            );
            assert_eq!(
                installer.reported(SpeechInput, report(4, LoadEvent::Failed)),
                []
            );
            assert_eq!(installer.view().speech_input, failed(ErrorKey::VoiceLoad));
            dropped(&mut installer);
        }
    }

    let mut removing = both_loading();
    assert_eq!(
        removing.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Intent { token: 3 }, vec![])
    );
    assert_eq!(
        removing.intended(SpeechInput, 3, Ok(())),
        [Effect::Unload {
            part: SpeechInput,
            ask: 1
        }]
    );
    dropped(&mut removing);
    assert_eq!(
        removing.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        [Effect::Delete {
            part: SpeechInput,
            token: 3
        }]
    );
    assert_eq!(removing.deleted(SpeechInput, 3, Folder::Present), []);
    dropped(&mut removing);
    assert_eq!(
        removing.view().speech_input,
        failed_absent(ErrorKey::VoiceStorage)
    );

    let mut quitting = both_loading();
    assert_eq!(
        quitting.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        quitting.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(quitting.quit(), []);
    dropped(&mut quitting);
    assert_eq!(quitting.view().speech_input, PartState::Unloaded);
}

#[test]
fn cancelling_an_explicit_idle_reload_waits_for_release_and_fences_late_success() {
    let mut installer = both_loading();
    assert_eq!(installer.cancel(), []);
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(
        installer.install(SpeechInput),
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(
        installer.cancel(),
        [Effect::Unload {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(installer.parts[0].durable, Durable::Installed);
    assert_eq!(installer.parts[0].models, [3]);
    assert_eq!(installer.view().speech_input, PartState::Loading);
    assert_eq!(installer.cancel(), []);
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(installer.parts[0].models, [3]);
    assert!(!installer.ready(SpeechInput));
    assert_eq!(installer.view().speech_input, PartState::Loading);
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(installer.parts[0].models, Vec::<u64>::new());
    assert_eq!(installer.parts[0].durable, Durable::Installed);
    assert_eq!(installer.view().speech_input, PartState::Unloaded);
    assert_eq!(
        installer.install(SpeechInput),
        [Effect::Load {
            part: SpeechInput,
            ask: 4
        }]
    );
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::Loading);
    assert_eq!(installer.parts[0].models, [4]);
    assert_eq!(
        installer.reported(SpeechInput, report(4, LoadEvent::Loaded)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::Ready);
}

#[test]
fn idle_release_during_a_refused_remove_is_replayed_before_a_later_reload() {
    use super::{Folder, RemoveAnswer};

    let mut installer = both_loading();
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Intent { token: 3 }, vec![])
    );
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::Ready);
    assert_eq!(
        installer.intended(SpeechInput, 3, Err(ErrorKey::SettingsRefused)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::Unloaded);
    assert_eq!(
        installer.install(SpeechInput),
        [Effect::Load {
            part: SpeechInput,
            ask: 4
        }]
    );
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(
        installer.reported(SpeechInput, report(4, LoadEvent::Failed)),
        []
    );
    assert_eq!(installer.view().speech_input, failed(ErrorKey::VoiceLoad));
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(installer.view().speech_input, failed(ErrorKey::VoiceLoad));
}

#[test]
fn quit_does_not_reload_a_dormant_installed_model() {
    let mut installer = both_loading();
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unloaded)),
        []
    );
    assert_eq!(installer.quit(), []);
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(installer.view().speech_input, PartState::Unloaded);
}

#[test]
fn start_up_loads_each_whole_installed_part_and_nothing_else() {
    let (installer, loads) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true, false],
        [true, true],
        &[],
    );
    assert_eq!(
        loads,
        [Effect::Load {
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
        &[],
    );
    assert_eq!(
        loads,
        [Effect::Load {
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
        []
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
        &[],
    );
    assert_eq!(loads, []);
    let fixed = view(
        failed(ErrorKey::VoiceLoad),
        failed_absent(ErrorKey::VoiceLoad),
    );
    assert_eq!(installer.view(), fixed);
    for part in [SpeechInput, SpokenReplies] {
        for ask in 1..=2 {
            assert_eq!(installer.reported(part, report(ask, LoadEvent::Loaded)), []);
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
        &[SpeechInput],
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
        []
    );
    assert!(installer.ready(SpokenReplies));
    assert!(!installer.ready(SpeechInput));
    assert_eq!(installer.view(), view(PartState::Loading, PartState::Ready));
}

#[test]
fn a_failed_load_is_a_load_failure_of_an_installed_part() {
    for event in [LoadEvent::Failed, LoadEvent::Discarded, LoadEvent::Unloaded] {
        let mut installer = both_loading();
        assert_eq!(installer.reported(SpeechInput, report(1, event)), []);
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
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(installer.view().speech_input, PartState::Loading);
    installer.reported(SpeechInput, report(3, LoadEvent::Loaded));
    assert!(installer.ready(SpeechInput));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Lost)),
        []
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
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(
        installer.reported(SpokenReplies, report(2, LoadEvent::Lost)),
        [Effect::Load {
            part: SpokenReplies,
            ask: 4
        }]
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
        assert_eq!(installer.reported(SpeechInput, report(1, event)), []);
    }
    assert!(installer.ready(SpeechInput), "the reload stays ready");
}

/// Neither part installed, both handles started; the next token is 3.
fn fresh() -> Installer {
    let (installer, effects) = Installer::start([Durable::Absent; 2], [true; 2], [true; 2], &[]);
    assert_eq!(effects, []);
    installer
}

fn fetch(part: VoicePart, token: u64) -> Effect {
    Effect::Fetch { part, token }
}

fn load(part: VoicePart, ask: u64) -> Effect {
    Effect::Load { part, ask }
}

#[test]
fn a_fresh_install_verifies_downloads_loads_and_is_ready_only_once_written() {
    let mut installer = fresh();
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 3)]);
    assert_eq!(
        installer.view(),
        view(PartState::Verifying, PartState::NotInstalled)
    );
    installer.progress(
        SpeechInput,
        3,
        Progress {
            phase: FetchPhase::Downloading,
            done: 3,
            total: 9,
        },
    );
    assert_eq!(
        installer.view().speech_input,
        PartState::Downloading { done: 3, total: 9 }
    );
    assert_eq!(
        installer.fetched(SpeechInput, 3, Ok(())),
        [load(SpeechInput, 3)]
    );
    assert_eq!(installer.view().speech_input, PartState::Loading);
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        [Effect::Activate {
            part: SpeechInput,
            token: 3
        }]
    );
    assert_eq!(
        installer.view().speech_input,
        PartState::Loading,
        "not before the write"
    );
    assert!(!installer.ready(SpeechInput));
    assert_eq!(installer.activated(SpeechInput, 3, Ok(())), []);
    assert_eq!(
        installer.view(),
        view(PartState::Ready, PartState::NotInstalled)
    );
    assert!(installer.ready(SpeechInput));
}

#[test]
fn a_whole_part_on_disk_goes_from_verifying_to_loading() {
    let mut installer = fresh();
    installer.install(SpokenReplies);
    installer.progress(
        SpokenReplies,
        3,
        Progress {
            phase: FetchPhase::Verifying,
            done: 9,
            total: 9,
        },
    );
    assert_eq!(installer.view().spoken_replies, PartState::Verifying);
    assert_eq!(
        installer.fetched(SpokenReplies, 3, Ok(())),
        [load(SpokenReplies, 3)]
    );
    assert_eq!(installer.view().spoken_replies, PartState::Loading);
}

#[test]
fn a_failed_fetch_shows_its_key_and_loads_nothing() {
    for (error, key) in [
        (DownloadError::NoSpace { needed: 1 }, ErrorKey::VoiceNoSpace),
        (DownloadError::Refused, ErrorKey::VoiceDownload),
        (DownloadError::Timeout, ErrorKey::VoiceDownload),
        (DownloadError::Transport, ErrorKey::VoiceDownload),
        (DownloadError::Cancelled, ErrorKey::VoiceDownload),
        (DownloadError::Size, ErrorKey::VoiceCorrupt),
        (DownloadError::Hash, ErrorKey::VoiceCorrupt),
        (DownloadError::Io, ErrorKey::VoiceStorage),
    ] {
        let mut installer = fresh();
        installer.install(SpeechInput);
        assert_eq!(
            installer.fetched(SpeechInput, 3, Err(error)),
            [],
            "{error:?}"
        );
        assert_eq!(
            installer.view().speech_input,
            failed_absent(key),
            "{error:?}"
        );
    }
}

#[test]
fn a_fresh_install_that_fails_to_load_is_not_installed_and_writes_nothing() {
    let mut installer = fresh();
    installer.install(SpeechInput);
    installer.fetched(SpeechInput, 3, Ok(()));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Failed)),
        []
    );
    assert_eq!(
        installer.view().speech_input,
        failed_absent(ErrorKey::VoiceLoad)
    );
}

#[test]
fn a_refused_activate_unloads_and_fails_uninstalled() {
    let mut installer = fresh();
    installer.install(SpokenReplies);
    installer.fetched(SpokenReplies, 3, Ok(()));
    installer.reported(SpokenReplies, report(3, LoadEvent::Loaded));
    assert_eq!(
        installer.activated(SpokenReplies, 3, Err(ErrorKey::SettingsRefused)),
        [Effect::Unload {
            part: SpokenReplies,
            ask: 3
        }]
    );
    assert_eq!(
        installer.view().spoken_replies,
        failed_absent(ErrorKey::SettingsRefused)
    );
    assert!(!installer.ready(SpokenReplies));
}

#[test]
fn a_loss_during_a_refused_activate_fails_and_writes_nothing_more() {
    let mut installer = fresh();
    installer.install(SpokenReplies);
    installer.fetched(SpokenReplies, 3, Ok(()));
    installer.reported(SpokenReplies, report(3, LoadEvent::Loaded));
    assert_eq!(
        installer.reported(SpokenReplies, report(3, LoadEvent::Lost)),
        []
    );
    assert_eq!(
        installer.activated(SpokenReplies, 3, Err(ErrorKey::SettingsRefused)),
        [Effect::Unload {
            part: SpokenReplies,
            ask: 3
        }]
    );
    assert_eq!(
        installer.reported(SpokenReplies, report(4, LoadEvent::Loaded)),
        []
    );
    assert_eq!(
        installer.view().spoken_replies,
        failed_absent(ErrorKey::SettingsRefused)
    );
    assert!(!installer.ready(SpokenReplies));
}

#[test]
fn a_loss_during_the_activate_reloads_once_it_commits() {
    let mut installer = fresh();
    installer.install(SpokenReplies);
    installer.fetched(SpokenReplies, 3, Ok(()));
    installer.reported(SpokenReplies, report(3, LoadEvent::Loaded));
    installer.reported(SpokenReplies, report(3, LoadEvent::Lost));
    assert_eq!(
        installer.activated(SpokenReplies, 3, Ok(())),
        [load(SpokenReplies, 4)]
    );
    assert_eq!(
        installer.reported(SpokenReplies, report(4, LoadEvent::Loaded)),
        []
    );
    assert!(installer.ready(SpokenReplies));
}

#[test]
fn a_loss_during_the_activate_does_not_reload_after_quit() {
    let mut installer = fresh();
    installer.install(SpokenReplies);
    installer.fetched(SpokenReplies, 3, Ok(()));
    installer.reported(SpokenReplies, report(3, LoadEvent::Loaded));
    installer.reported(SpokenReplies, report(3, LoadEvent::Lost));
    assert_eq!(installer.quit(), []);
    assert_eq!(installer.activated(SpokenReplies, 3, Ok(())), []);
    assert_eq!(installer.view().spoken_replies, failed(ErrorKey::VoiceLoad));
    assert!(!installer.ready(SpokenReplies));
}

#[test]
fn a_second_install_waits_for_the_first_fetch() {
    let mut installer = fresh();
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 3)]);
    assert_eq!(installer.install(SpokenReplies), []);
    assert_eq!(
        installer.view(),
        view(PartState::Verifying, PartState::Verifying)
    );
    assert_eq!(
        installer.fetched(SpeechInput, 3, Ok(())),
        [load(SpeechInput, 3), fetch(SpokenReplies, 4)]
    );
    assert_eq!(
        installer.fetched(SpeechInput, 3, Ok(())),
        [],
        "a repeated completion is stale"
    );
}

#[test]
fn a_cancelled_fetch_ends_before_the_next_one_starts() {
    let mut installer = fresh();
    installer.install(SpeechInput);
    assert_eq!(installer.cancel(), [Effect::CancelFetch]);
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    assert_eq!(
        installer.install(SpeechInput),
        [],
        "the cancelled fetch still holds the part's files"
    );
    assert_eq!(installer.view().speech_input, PartState::Verifying);
    assert_eq!(
        installer.fetched(SpeechInput, 3, Err(DownloadError::Cancelled)),
        [fetch(SpeechInput, 5)]
    );
}

#[test]
fn a_cancelled_load_goes_back_and_its_late_success_is_dropped() {
    let mut installer = fresh();
    installer.install(SpeechInput);
    installer.fetched(SpeechInput, 3, Ok(()));
    assert_eq!(
        installer.cancel(),
        [Effect::Unload {
            part: SpeechInput,
            ask: 3
        }]
    );
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 5)]);
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        [],
        "a stale success sends nothing, not even an unload"
    );
    assert_eq!(installer.view().speech_input, PartState::Verifying);
    installer.fetched(SpeechInput, 5, Ok(()));
    installer.reported(SpeechInput, report(5, LoadEvent::Loaded));
    installer.activated(SpeechInput, 5, Ok(()));
    assert!(installer.ready(SpeechInput));
}

#[test]
fn a_commit_in_flight_isnt_cancelled() {
    let mut installer = fresh();
    installer.install(SpeechInput);
    installer.fetched(SpeechInput, 3, Ok(()));
    installer.reported(SpeechInput, report(3, LoadEvent::Loaded));
    assert_eq!(installer.cancel(), []);
    assert_eq!(installer.view().speech_input, PartState::Loading);
    assert_eq!(installer.activated(SpeechInput, 3, Ok(())), []);
    assert!(installer.ready(SpeechInput));
}

#[test]
fn cancel_sends_every_install_back_to_where_it_was() {
    let mut installer = fresh();
    installer.install(SpeechInput);
    installer.fetched(SpeechInput, 3, Err(DownloadError::Timeout));
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 4)]);
    assert_eq!(installer.install(SpokenReplies), []);
    assert_eq!(installer.cancel(), [Effect::CancelFetch]);
    assert_eq!(
        installer.view(),
        view(
            failed_absent(ErrorKey::VoiceDownload),
            PartState::NotInstalled
        )
    );
    assert_eq!(
        installer.fetched(SpeechInput, 4, Err(DownloadError::Cancelled)),
        [],
        "nothing is left queued"
    );
}

#[test]
fn a_fresh_install_lost_twice_is_not_installed_and_a_retry_gets_a_new_reload() {
    let mut installer = fresh();
    installer.install(SpeechInput);
    installer.fetched(SpeechInput, 3, Ok(()));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Lost)),
        [load(SpeechInput, 4)]
    );
    assert_eq!(
        installer.reported(SpeechInput, report(4, LoadEvent::Lost)),
        []
    );
    assert_eq!(
        installer.view().speech_input,
        failed_absent(ErrorKey::VoiceLoad)
    );
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 6)]);
    installer.fetched(SpeechInput, 6, Ok(()));
    assert_eq!(
        installer.reported(SpeechInput, report(6, LoadEvent::Lost)),
        [load(SpeechInput, 7)]
    );
}

#[test]
fn a_retry_of_an_installed_part_is_ready_without_a_write() {
    let (mut installer, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [false, true],
        [true, true],
        &[],
    );
    assert_eq!(effects, []);
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 3)]);
    installer.fetched(SpeechInput, 3, Ok(()));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        []
    );
    assert!(installer.ready(SpeechInput));
}

#[test]
fn a_removal_whose_last_write_fails_stays_removing_and_installs_nothing() {
    let (mut installer, effects) = Installer::start(
        [Durable::Removing, Durable::Absent],
        [true, true],
        [true, true],
        &[],
    );
    assert_eq!(effects, []);
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    assert_eq!(
        installer.cleared(SpeechInput, Err(ErrorKey::SettingsRefused)),
        []
    );
    assert_eq!(
        installer.view().speech_input,
        failed_absent(ErrorKey::VoiceStorage)
    );
    assert_eq!(
        installer.install(SpeechInput),
        [Effect::Delete {
            part: SpeechInput,
            token: 3
        }],
        "the cleanup is retried before anything fetches"
    );
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
}

#[test]
fn an_install_during_a_removals_last_write_starts_once_it_commits() {
    let (mut installer, _) = Installer::start(
        [Durable::Removing, Durable::Absent],
        [true, true],
        [true, true],
        &[],
    );
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    assert_eq!(
        installer.cleared(SpeechInput, Ok(())),
        [fetch(SpeechInput, 3)]
    );
    assert_eq!(installer.view().speech_input, PartState::Verifying);
}

/// L2: both parts waiting behind a cancelled fetch start in the order they were asked for, not in
/// part order.
#[test]
fn installs_waiting_behind_a_cancelled_fetch_start_in_request_order() {
    let mut installer = fresh();
    assert_eq!(installer.install(SpeechInput), [fetch(SpeechInput, 3)]);
    assert_eq!(installer.cancel(), [Effect::CancelFetch]);
    assert_eq!(installer.install(SpokenReplies), []);
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(
        installer.view(),
        view(PartState::Verifying, PartState::Verifying)
    );
    assert_eq!(
        installer.fetched(SpeechInput, 3, Err(DownloadError::Cancelled)),
        [fetch(SpokenReplies, 5)]
    );
    assert_eq!(
        installer.fetched(SpokenReplies, 5, Ok(())),
        [load(SpokenReplies, 5), fetch(SpeechInput, 6)]
    );
}
