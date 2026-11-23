//! The installer's removal rules: nothing happens before the intent commits, the part drains
//! before its folder is deleted, and a failed removal is retried by the next install or remove.

use kumochi_protocol::message::{ErrorKey, PartState};
use kumochi_protocol::voice::VoicePart::{self, SpeechInput, SpokenReplies};

use super::{Durable, Effect, Folder, Installer, RemoveAnswer};
use crate::voice::download::{DownloadError, FetchPhase, Progress};
use crate::voice::{LoadEvent, LoadReport};

#[test]
fn an_unsupported_delete_keeps_its_debt_without_retry_or_remove_done() {
    let (mut installer, _) =
        Installer::start([Durable::Absent; 2], [Ok(false); 2], [true; 2], &[], true);
    let part = SpeechInput;
    assert_eq!(
        installer.remove(part, Folder::Present),
        (RemoveAnswer::Intent { token: 3 }, vec![])
    );
    assert_eq!(
        installer.intended(part, 3, Ok(())),
        [Effect::Delete { part, token: 3 }]
    );
    assert_eq!(
        installer.deleted(part, 3, Folder::Refused(ErrorKey::UnsupportedPlatform)),
        []
    );
    assert_eq!(
        installer.view().speech_input,
        PartState::Failed {
            key: ErrorKey::UnsupportedPlatform,
            installed: false
        }
    );
    assert_eq!(installer.install(part), []);
    assert_eq!(
        installer.remove(part, Folder::Present),
        (RemoveAnswer::Unsupported, vec![])
    );
}

#[test]
fn a_refused_removal_releases_the_restored_input_if_off_without_claiming_unknown_exit() {
    let mut installer = ready();
    intent(&mut installer, 3);
    assert_eq!(installer.set_input_enabled(false), []);
    assert_eq!(
        installer.intended(SpeechInput, 3, Err(ErrorKey::SettingsRefused)),
        [Effect::Unload {
            part: SpeechInput,
            ask: 1
        }]
    );
    assert_eq!(installer.view().speech_input, PartState::Loading);
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Unavailable)),
        []
    );
    assert_eq!(installer.view().speech_input, PartState::Unavailable);
    assert_eq!(installer.set_input_enabled(true), []);
    assert_eq!(installer.reload_input(), []);
}

fn report(ask: u64, event: LoadEvent) -> LoadReport {
    LoadReport { ask, event }
}

fn storage_failure() -> PartState {
    PartState::Failed {
        key: ErrorKey::VoiceStorage,
        installed: false,
    }
}

fn delete(part: VoicePart, token: u64) -> Effect {
    Effect::Delete { part, token }
}

fn unload(part: VoicePart, ask: u64) -> Effect {
    Effect::Unload { part, ask }
}

/// Speech input ready from start-up with ask 1; the next token is 3.
fn ready() -> Installer {
    let (mut installer, _) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true, true].map(Ok),
        [true, true],
        &[],
        true,
    );
    installer.reported(SpeechInput, report(1, LoadEvent::Loaded));
    assert!(installer.ready(SpeechInput));
    installer
}

/// Nothing installed; speech input's fetch has token 3 and is downloading.
fn fetching() -> Installer {
    let (mut installer, _) = Installer::start(
        [Durable::Absent; 2],
        [true; 2].map(Ok),
        [true; 2],
        &[],
        true,
    );
    assert_eq!(
        installer.install(SpeechInput),
        [Effect::Fetch {
            part: SpeechInput,
            token: 3
        }]
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
    installer
}

/// As [`fetching`], with the fetch done and ask 3 loading.
fn loading() -> Installer {
    let mut installer = fetching();
    assert_eq!(
        installer.fetched(SpeechInput, 3, Ok(())),
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }]
    );
    installer
}

/// A removal's intent with token `token`, which changes nothing yet.
fn intent(installer: &mut Installer, token: u64) {
    let before = installer.view();
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Intent { token }, Vec::new())
    );
    assert_eq!(installer.view(), before, "nothing shows before the commit");
}

/// Row 6: a committed removal unloads, waits for the ack, deletes, then clears.
#[test]
fn a_committed_removal_deletes_only_after_the_unload_ack() {
    for terminal in [LoadEvent::Unloaded, LoadEvent::Lost] {
        let mut installer = ready();
        intent(&mut installer, 3);
        assert!(
            installer.ready(SpeechInput),
            "still served until the commit"
        );
        assert_eq!(
            installer.intended(SpeechInput, 3, Ok(())),
            [unload(SpeechInput, 1)]
        );
        assert_eq!(installer.view().speech_input, PartState::NotInstalled);
        assert!(!installer.ready(SpeechInput));
        assert_eq!(
            installer.reported(SpeechInput, report(1, terminal)),
            [delete(SpeechInput, 3)]
        );
        assert_eq!(
            installer.deleted(SpeechInput, 3, Folder::Missing),
            [Effect::Clear { part: SpeechInput }]
        );
        assert_eq!(installer.cleared(SpeechInput, Ok(())), []);
        assert_eq!(installer.view().speech_input, PartState::NotInstalled);
        assert_eq!(
            installer.install(SpeechInput),
            [Effect::Fetch {
                part: SpeechInput,
                token: 4
            }],
            "removed, so the next install is a fresh one"
        );
    }
}

/// Row 5: a refused intent leaves the model loaded and the part as it was, on its old token.
#[test]
fn a_refused_intent_keeps_the_part_and_its_token() {
    let mut installer = ready();
    intent(&mut installer, 3);
    assert_eq!(
        installer.intended(SpeechInput, 3, Err(ErrorKey::SettingsRefused)),
        []
    );
    assert!(installer.ready(SpeechInput));
    assert_eq!(
        installer.reported(SpeechInput, report(1, LoadEvent::Lost)),
        [Effect::Load {
            part: SpeechInput,
            ask: 4
        }],
        "a loss of the old ask still counts"
    );
}

/// Row 7: a fetch is cancelled only once the intent commits, and the delete waits for it to end.
#[test]
fn a_removal_during_a_fetch_cancels_it_only_after_the_commit() {
    let mut installer = fetching();
    intent(&mut installer, 4);
    assert_eq!(
        installer.intended(SpeechInput, 4, Ok(())),
        [Effect::CancelFetch]
    );
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    assert_eq!(
        installer.fetched(SpeechInput, 3, Err(DownloadError::Cancelled)),
        [delete(SpeechInput, 4)]
    );
}

#[test]
fn cancel_fences_a_fetch_even_if_the_removal_intent_is_refused() {
    for fetch_completed in [false, true] {
        let mut installer = fetching();
        intent(&mut installer, 4);
        if fetch_completed {
            assert_eq!(installer.fetched(SpeechInput, 3, Ok(())), []);
        }
        let expected = if fetch_completed {
            Vec::new()
        } else {
            vec![Effect::CancelFetch]
        };
        assert_eq!(installer.cancel(), expected);
        assert_eq!(
            installer.intended(SpeechInput, 4, Err(ErrorKey::SettingsRefused)),
            [],
            "a refused removal cannot resume the cancelled install"
        );
        if !fetch_completed {
            assert_eq!(installer.fetched(SpeechInput, 3, Ok(())), []);
        }
        assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    }
}

/// Row 31: a load held during a committed intent is never applied; its model is unloaded and the
/// delete waits for that.
#[test]
fn a_load_held_during_a_committed_intent_is_unloaded_before_the_delete() {
    let mut installer = loading();
    intent(&mut installer, 4);
    installer.reported(SpeechInput, report(3, LoadEvent::Loaded));
    assert_eq!(
        installer.intended(SpeechInput, 4, Ok(())),
        [unload(SpeechInput, 3)]
    );
    assert!(!installer.ready(SpeechInput));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Unloaded)),
        [delete(SpeechInput, 4)]
    );
}

/// Row 8: a load still running when the removal commits is never ready, and its discard ends
/// the drain.
#[test]
fn a_removal_during_a_load_is_never_ready() {
    let mut installer = loading();
    intent(&mut installer, 4);
    assert_eq!(
        installer.intended(SpeechInput, 4, Ok(())),
        [unload(SpeechInput, 3)]
    );
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        [],
        "a Loaded already sent doesn't end the drain"
    );
    assert!(!installer.ready(SpeechInput));
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Discarded)),
        [delete(SpeechInput, 4)]
    );
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
}

/// An Activate result held during a refused intent still makes the part ready.
#[test]
fn a_commit_held_during_a_refused_intent_is_ready_once_its_refused() {
    let mut installer = loading();
    installer.reported(SpeechInput, report(3, LoadEvent::Loaded));
    intent(&mut installer, 4);
    assert_eq!(installer.activated(SpeechInput, 3, Ok(())), [], "held");
    assert_eq!(
        installer.intended(SpeechInput, 4, Err(ErrorKey::SettingsRefused)),
        []
    );
    assert!(installer.ready(SpeechInput));
}

fn install_during_held_delete(part: VoicePart) -> Installer {
    let (mut installer, _) = Installer::start(
        [Durable::Absent; 2],
        [false; 2].map(Ok),
        [true; 2],
        &[],
        true,
    );
    assert_eq!(
        installer.remove(part, Folder::Present),
        (RemoveAnswer::Intent { token: 3 }, Vec::new())
    );
    assert_eq!(installer.intended(part, 3, Ok(())), [delete(part, 3)]);
    assert_eq!(installer.install(part), []);
    installer
}

#[test]
fn an_install_during_the_delete_fetches_once_the_removal_finishes() {
    for part in VoicePart::ALL {
        let mut installer = install_during_held_delete(part);
        assert_eq!(
            installer.deleted(part, 3, Folder::Missing),
            [Effect::Clear { part }]
        );
        assert_eq!(
            installer.cleared(part, Ok(())),
            [Effect::Fetch { part, token: 4 }]
        );
    }
}

fn cancel_during_held_delete(part: VoicePart) {
    let mut installer = install_during_held_delete(part);
    assert_eq!(installer.cancel(), []);
    assert_eq!(
        installer.deleted(part, 3, Folder::Missing),
        [Effect::Clear { part }],
        "the removal still finishes"
    );
    assert_eq!(
        installer.cleared(part, Ok(())),
        [],
        "the cancelled install must not fetch"
    );
    let view = installer.view();
    assert_eq!(view.speech_input, PartState::NotInstalled);
    assert_eq!(view.spoken_replies, PartState::NotInstalled);
}

#[test]
fn cancel_during_held_delete_drops_speech_input_install() {
    cancel_during_held_delete(SpeechInput);
}

#[test]
fn cancel_during_held_delete_drops_spoken_replies_install() {
    cancel_during_held_delete(SpokenReplies);
}

/// An install during a refused intent runs once it's refused.
#[test]
fn an_install_during_a_refused_intent_runs_once_its_refused() {
    let (mut installer, _) = Installer::start(
        [Durable::Absent; 2],
        [true; 2].map(Ok),
        [true; 2],
        &[],
        true,
    );
    intent(&mut installer, 3);
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(
        installer.intended(SpeechInput, 3, Err(ErrorKey::SettingsRefused)),
        [Effect::Fetch {
            part: SpeechInput,
            token: 4
        }]
    );
}

/// Row 13: a start-up delete that failed is retried by the next install, which fetches only
/// once the removal is written as finished.
#[test]
fn an_install_after_a_failed_start_up_delete_retries_it_first() {
    let (mut installer, effects) = Installer::start(
        [Durable::Removing, Durable::Absent],
        [true, true].map(Ok),
        [true, true],
        &[(SpeechInput, Folder::Present)],
        true,
    );
    assert_eq!(effects, []);
    assert_eq!(installer.view().speech_input, storage_failure());
    assert_eq!(installer.install(SpeechInput), [delete(SpeechInput, 3)]);
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    assert_eq!(
        installer.deleted(SpeechInput, 3, Folder::Missing),
        [Effect::Clear { part: SpeechInput }]
    );
    assert_eq!(
        installer.cleared(SpeechInput, Ok(())),
        [Effect::Fetch {
            part: SpeechInput,
            token: 4
        }]
    );
}

/// Row 32: a failed delete stays in cleanup and drops the install waiting for it; a remove from
/// there retries the same cleanup with no second intent.
#[test]
fn a_remove_after_a_failed_delete_retries_it_without_a_new_intent() {
    let mut installer = ready();
    intent(&mut installer, 3);
    installer.intended(SpeechInput, 3, Ok(()));
    installer.reported(SpeechInput, report(1, LoadEvent::Unloaded));
    assert_eq!(installer.install(SpeechInput), []);
    assert_eq!(installer.deleted(SpeechInput, 3, Folder::Present), []);
    assert_eq!(installer.view().speech_input, storage_failure());
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Accepted, vec![delete(SpeechInput, 5)])
    );
    assert_eq!(installer.view().speech_input, PartState::NotInstalled);
    installer.deleted(SpeechInput, 5, Folder::Missing);
    assert_eq!(
        installer.cleared(SpeechInput, Ok(())),
        [],
        "the install that waited for the failed delete doesn't run"
    );
}

/// Row 14: a failed `RemoveDone` stays in cleanup, and an install retries the delete and the
/// write before it fetches.
#[test]
fn an_install_after_a_failed_remove_done_retries_the_cleanup_first() {
    let mut installer = ready();
    intent(&mut installer, 3);
    installer.intended(SpeechInput, 3, Ok(()));
    installer.reported(SpeechInput, report(1, LoadEvent::Unloaded));
    installer.deleted(SpeechInput, 3, Folder::Missing);
    assert_eq!(
        installer.cleared(SpeechInput, Err(ErrorKey::SettingsRefused)),
        []
    );
    assert_eq!(installer.view().speech_input, storage_failure());
    assert_eq!(installer.install(SpeechInput), [delete(SpeechInput, 4)]);
    assert_eq!(
        installer.deleted(SpeechInput, 4, Folder::Missing),
        [Effect::Clear { part: SpeechInput }]
    );
    assert_eq!(
        installer.cleared(SpeechInput, Ok(())),
        [Effect::Fetch {
            part: SpeechInput,
            token: 5
        }]
    );
}

/// A part with nothing installed or on disk has nothing to remove; a queued install is dropped.
#[test]
fn a_removal_of_nothing_is_refused_and_drops_a_queued_install() {
    let mut installer = fetching();
    assert_eq!(installer.install(SpokenReplies), []);
    assert_eq!(installer.view().spoken_replies, PartState::Verifying);
    assert_eq!(
        installer.remove(SpokenReplies, Folder::Missing),
        (RemoveAnswer::Refused, Vec::new())
    );
    assert_eq!(installer.view().spoken_replies, PartState::NotInstalled);
    assert_eq!(
        installer.fetched(SpeechInput, 3, Ok(())),
        [Effect::Load {
            part: SpeechInput,
            ask: 3
        }],
        "nothing fetches for the dropped install"
    );
}

/// A removal already under way answers accepted and starts nothing new.
#[test]
fn a_second_removal_is_accepted_and_changes_nothing() {
    let mut installer = ready();
    intent(&mut installer, 3);
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Accepted, Vec::new())
    );
    installer.intended(SpeechInput, 3, Ok(()));
    assert_eq!(
        installer.remove(SpeechInput, Folder::Present),
        (RemoveAnswer::Accepted, Vec::new())
    );
    assert_eq!(installer.cancel(), [], "a removal isn't cancelled");
}

/// A fetch a cancel let go of still runs, so a removal after the cancel deletes only once it ends.
#[test]
fn a_removal_after_a_cancelled_fetch_waits_for_that_fetch_to_end() {
    let mut installer = fetching();
    assert_eq!(installer.cancel(), [Effect::CancelFetch]);
    intent(&mut installer, 5);
    assert_eq!(
        installer.intended(SpeechInput, 5, Ok(())),
        [Effect::CancelFetch]
    );
    assert_eq!(
        installer.fetched(SpeechInput, 3, Err(DownloadError::Cancelled)),
        [delete(SpeechInput, 5)]
    );
}

/// A refused Activate unloads its model, which stays until its report, so a removal waits too.
#[test]
fn a_removal_after_a_refused_activate_waits_for_the_unload() {
    let mut installer = loading();
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
        [Effect::Activate {
            part: SpeechInput,
            token: 3
        }]
    );
    assert_eq!(
        installer.activated(SpeechInput, 3, Err(ErrorKey::SettingsRefused)),
        [unload(SpeechInput, 3)]
    );
    intent(&mut installer, 5);
    assert_eq!(
        installer.intended(SpeechInput, 5, Ok(())),
        [unload(SpeechInput, 3)]
    );
    assert_eq!(
        installer.reported(SpeechInput, report(3, LoadEvent::Unloaded)),
        [delete(SpeechInput, 5)]
    );
    assert_eq!(
        installer.deleted(SpeechInput, 5, Folder::Missing),
        [Effect::Clear { part: SpeechInput }]
    );
}

/// A fetch that ends while its part's intent is written frees the one fetch, so the other part's
/// queued install starts then, once, whichever way the intent goes.
#[test]
fn a_fetch_held_during_an_intent_starts_the_other_parts_install() {
    for refused in [false, true] {
        let mut installer = fetching();
        assert_eq!(installer.install(SpokenReplies), []);
        intent(&mut installer, 5);
        assert_eq!(
            installer.fetched(SpeechInput, 3, Ok(())),
            [Effect::Fetch {
                part: SpokenReplies,
                token: 4
            }],
            "refused: {refused}"
        );
        let (result, effect) = if refused {
            (
                Err(ErrorKey::SettingsRefused),
                Effect::Load {
                    part: SpeechInput,
                    ask: 3,
                },
            )
        } else {
            (Ok(()), delete(SpeechInput, 5))
        };
        assert_eq!(
            installer.intended(SpeechInput, 5, result),
            [effect],
            "refused: {refused}"
        );
        if refused {
            assert_eq!(installer.view().speech_input, PartState::Loading);
            assert_eq!(
                installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
                [Effect::Activate {
                    part: SpeechInput,
                    token: 3
                }]
            );
            assert_eq!(installer.activated(SpeechInput, 3, Ok(())), []);
            assert!(installer.ready(SpeechInput));
        }
    }
}

/// Completions held during a refused intent are applied in the order they came: a load, then its
/// loss, commits first and reloads once the commit lands.
#[test]
fn completions_held_during_a_refused_intent_apply_in_arrival_order() {
    for lost in [true, false] {
        let mut installer = loading();
        intent(&mut installer, 4);
        assert_eq!(
            installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
            []
        );
        if lost {
            assert_eq!(
                installer.reported(SpeechInput, report(3, LoadEvent::Lost)),
                []
            );
        }
        assert_eq!(
            installer.intended(SpeechInput, 4, Err(ErrorKey::SettingsRefused)),
            [Effect::Activate {
                part: SpeechInput,
                token: 3
            }]
        );
        let effects = installer.activated(SpeechInput, 3, Ok(()));
        if lost {
            assert_eq!(
                effects,
                [Effect::Load {
                    part: SpeechInput,
                    ask: 5
                }]
            );
            assert_eq!(installer.view().speech_input, PartState::Loading);
        } else {
            assert_eq!(effects, []);
            assert_eq!(installer.view().speech_input, PartState::Ready);
        }
    }
}

#[test]
fn a_newer_unload_ack_doesnt_release_an_older_cancelled_load() {
    for newer in [false, true] {
        let mut installer = loading();
        assert_eq!(installer.cancel(), [unload(SpeechInput, 3)]);
        let token = if newer {
            assert_eq!(
                installer.install(SpeechInput),
                [Effect::Fetch {
                    part: SpeechInput,
                    token: 5
                }]
            );
            assert_eq!(
                installer.fetched(SpeechInput, 5, Ok(())),
                [Effect::Load {
                    part: SpeechInput,
                    ask: 5
                }]
            );
            6
        } else {
            5
        };
        intent(&mut installer, token);
        let mut expected = vec![unload(SpeechInput, 3)];
        if newer {
            expected.push(unload(SpeechInput, 5));
        }
        assert_eq!(installer.intended(SpeechInput, token, Ok(())), expected);
        if newer {
            assert_eq!(
                installer.reported(SpeechInput, report(5, LoadEvent::Unloaded)),
                []
            );
        }
        assert_eq!(
            installer.reported(SpeechInput, report(3, LoadEvent::Loaded)),
            []
        );
        assert_eq!(
            installer.reported(SpeechInput, report(3, LoadEvent::Discarded)),
            [delete(SpeechInput, token)]
        );
        assert_eq!(
            installer.reported(SpeechInput, report(3, LoadEvent::Unloaded)),
            [],
            "a duplicate terminal report cannot issue a second delete"
        );
    }
}
