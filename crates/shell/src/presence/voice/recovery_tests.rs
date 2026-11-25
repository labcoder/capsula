use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{DataToShell, Incarnation, RunId};

use super::super::PresenceEvent;
use super::super::voice_wiring_tests::{hold, ready, status};
use super::tests::{after, native};
use super::*;
use crate::input::{Event, hold::Intent};
use crate::link::{Batch, LinkEvent, LinkState, Session};

const SENTINEL: &str = "zqxsentinellabel held words";

fn assert_redacted(printed: &str) {
    let marker = "zqxsentinellabel";
    let list = format!("{:?}", marker.as_bytes());
    assert!(!printed.contains(marker), "{printed}");
    assert!(!printed.contains(&list[1..list.len() - 1]), "{printed}");
}

fn run_state(phase: Phase) -> LinkEvent {
    LinkEvent::RunState {
        generation: 1,
        run: RunId(1),
        thread: ThreadId(1),
        phase,
        error: None,
        origin: None,
    }
}

/// The voice notice as the status panel and the composer show it.
fn shown(owner: &Entity<Presence>, cx: &mut TestAppContext) -> (Option<Text>, Option<String>) {
    owner.read_with(cx, |owner, cx| {
        let composer = owner.text.as_ref().expect("typing fixture").view.read(cx);
        (
            owner.panel.read(cx).voice,
            composer.notice.as_ref().map(ToString::to_string),
        )
    })
}

/// A hold whose words went back into the composer when the brain was lost.
fn recovered(cx: &mut TestAppContext) -> Entity<Presence> {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    let strings = Strings::english();
    assert_eq!(
        shown(&owner, cx),
        (
            Some(Text::HoldLostRestored),
            Some(strings.get(Text::HoldLostRestored))
        )
    );
    owner
}

fn deliver(owner: &Entity<Presence>, event: LinkEvent, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(vec![event]), cx)
            .expect("link batch");
    });
}

fn words(generation: u64, gesture: u32, text: &str, bytes: u32) -> LinkEvent {
    LinkEvent::Data {
        generation,
        message: DataToShell::HoldWords {
            gesture: GestureId(gesture),
            words: text.into(),
        },
        bytes,
    }
}

fn stored(generation: u64, gesture: u32) -> LinkEvent {
    LinkEvent::Data {
        generation,
        message: DataToShell::HoldStored {
            gesture: GestureId(gesture),
        },
        bytes: 6,
    }
}

/// The hold was released and sent, and the brain started its run.
fn thinking(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, _| {
        owner.voice.state.pending.as_mut().expect("a hold").source = Source::Sent;
    });
    deliver(
        owner,
        LinkEvent::RunState {
            generation: 1,
            run: RunId(1),
            thread: ThreadId(1),
            phase: Phase::Thinking,
            error: None,
            origin: Some(RunOrigin::Hold {
                gesture: GestureId(1),
            }),
        },
        cx,
    );
}

fn draft(owner: &Entity<Presence>, cx: &mut TestAppContext) -> String {
    owner.read_with(cx, |owner, cx| {
        owner
            .text
            .as_ref()
            .expect("typing fixture")
            .input
            .read(cx)
            .value()
            .to_string()
    })
}

fn set_draft(owner: &Entity<Presence>, text: &'static str, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        let text_panel = owner.text.as_ref().expect("typing fixture");
        let input = text_panel.input.clone();
        text_panel
            .window
            .update(cx, |_, window, cx| {
                input.update(cx, |input, cx| input.set_value(text, window, cx));
            })
            .expect("typing window");
    });
}

fn granted(owner: &Entity<Presence>, cx: &mut TestAppContext) -> u32 {
    owner.read_with(cx, |owner, _| {
        owner.session.as_ref().expect("connected").granted()
    })
}

fn lost(owner: &Entity<Presence>, cx: &mut TestAppContext) -> Option<Text> {
    owner.read_with(cx, |owner, _| owner.voice.state.lost)
}

#[gpui::test]
fn a_brain_lost_before_the_receipt_restores_the_words_once_and_keeps_the_notice(
    cx: &mut TestAppContext,
) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    thinking(&owner, cx);
    let shown = owner.read_with(cx, |owner, _| {
        assert!(
            owner.voice.state.recovery.is_some(),
            "Thinking keeps the copy"
        );
        owner.text.as_ref().expect("typing fixture").typing.visible
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), SENTINEL);
    assert_eq!(lost(&owner, cx), Some(Text::HoldLostRestored));
    owner.read_with(cx, |owner, cx| {
        assert!(owner.voice.state.recovery.is_none());
        assert_eq!(
            owner.text.as_ref().expect("typing fixture").typing.visible,
            shown
        );
        assert_eq!(owner.panel.read(cx).voice, Some(Text::HoldLostRestored));
    });
    set_draft(&owner, "", cx);
    deliver(&owner, LinkEvent::State(LinkState::Failed), cx);
    owner.update(cx, |owner, cx| {
        owner.on_event(PresenceEvent::Retry, cx).expect("retry");
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    deliver(
        &owner,
        LinkEvent::Connected(Session::detached(2, Incarnation(1))),
        cx,
    );
    assert_eq!(draft(&owner, cx), "", "the words come back only once");
    assert_eq!(lost(&owner, cx), Some(Text::HoldLostRestored));
    owner.read_with(cx, |owner, cx| {
        assert_eq!(owner.panel.read(cx).voice, Some(Text::HoldLostRestored));
    });
}

#[gpui::test]
fn only_a_revoked_holds_words_stay_back_after_a_loss(cx: &mut TestAppContext) {
    use kumochi_protocol::message::{ControlToBrain, SettingsOp};

    // (how the hold's input ends before the brain is lost, whether the words come back)
    for (case, restored) in [("change", false), ("escape", false), ("answered", true)] {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        deliver(&owner, words(1, 1, SENTINEL, 40), cx);
        if case == "answered" {
            thinking(&owner, cx);
            deliver(&owner, run_state(Phase::Answering), cx);
        }
        owner.update(cx, |owner, _| {
            if case == "escape" {
                assert!(owner.link.cancel_input());
            } else {
                let session = owner.session.clone().expect("connected");
                let off = ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false));
                assert!(owner.link.send_control(&session, off));
            }
        });
        deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(
            draft(&owner, cx),
            if restored { SENTINEL } else { "" },
            "{case}"
        );
        assert_eq!(
            lost(&owner, cx),
            restored.then_some(Text::HoldLostRestored),
            "{case}"
        );
    }
}

#[gpui::test]
fn the_stored_receipt_consumes_the_copy_and_a_later_loss_restores_nothing(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    thinking(&owner, cx);
    let before = granted(&owner, cx);
    deliver(&owner, stored(1, 1), cx);
    assert_eq!(granted(&owner, cx), before + 6);
    owner.read_with(cx, |owner, _| assert!(owner.voice.state.recovery.is_none()));
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
    assert_eq!(lost(&owner, cx), None);
}

#[gpui::test]
fn words_after_a_terminal_status_or_from_another_hold_are_refused_with_their_credit(
    cx: &mut TestAppContext,
) {
    for terminal in [
        HoldStatus::Empty,
        HoldStatus::Cancelled,
        HoldStatus::Failed(HoldFailure::TimedOut),
    ] {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        status(&owner, 1, 1, terminal, cx);
        let before = granted(&owner, cx);
        deliver(&owner, words(1, 1, SENTINEL, 40), cx);
        assert_eq!(granted(&owner, cx), before + 40);
        owner.read_with(cx, |owner, _| assert!(owner.voice.state.recovery.is_none()));
        deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(draft(&owner, cx), "");
        assert_eq!(lost(&owner, cx), None);
        deliver(
            &owner,
            LinkEvent::Connected(Session::detached(2, Incarnation(1))),
            cx,
        );
        assert_eq!(draft(&owner, cx), "");
        assert_eq!(lost(&owner, cx), None);
    }
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(0, 1, SENTINEL, 40), cx);
    deliver(&owner, words(1, 2, SENTINEL, 40), cx);
    deliver(&owner, stored(0, 1), cx);
    deliver(&owner, stored(1, 2), cx);
    owner.read_with(cx, |owner, _| {
        let slot = owner
            .voice
            .state
            .recovery
            .as_ref()
            .expect("the live hold's slot");
        assert!(slot.words.is_none(), "another connection's or hold's words");
    });
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    deliver(&owner, words(1, 1, "a second copy", 30), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), SENTINEL);
}

#[gpui::test]
fn a_failed_or_stopped_held_run_drops_its_words(cx: &mut TestAppContext) {
    for (phase, answered) in [
        (Phase::Failed, false),
        (Phase::Interrupted, false),
        (Phase::Failed, true),
        (Phase::Interrupted, true),
    ] {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        thinking(&owner, cx);
        if answered {
            deliver(&owner, run_state(Phase::Answering), cx);
            owner.read_with(cx, |owner, _| assert!(owner.voice.state.pending.is_none()));
        }
        deliver(&owner, run_state(phase), cx);
        owner.read_with(cx, |owner, _| assert!(owner.voice.state.recovery.is_none()));
        let before = granted(&owner, cx);
        deliver(&owner, words(1, 1, SENTINEL, 40), cx);
        assert_eq!(granted(&owner, cx), before + 40);
        deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(draft(&owner, cx), "", "{phase:?} answered={answered}");
        assert_eq!(lost(&owner, cx), None);
    }
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    thinking(&owner, cx);
    deliver(&owner, run_state(Phase::Answering), cx);
    owner.update(cx, |owner, _| {
        // A newer hold's slot, with words and no run yet.
        owner.voice.state.recovery = Some(Recovery {
            session: 1,
            gesture: GestureId(2),
            run: None,
            target: owner.voice.target.get(),
            picture: false,
            words: Some(SENTINEL.into()),
        });
    });
    deliver(&owner, run_state(Phase::Failed), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), SENTINEL, "an older run's failure");
}

#[gpui::test]
fn a_newer_attempt_or_a_stuck_microphone_replaces_the_lost_notice(cx: &mut TestAppContext) {
    let strings = Strings::english();
    let owner = recovered(cx);
    cx.run_until_parked();
    owner.update(cx, |owner, cx| {
        crate::presence::voice_wiring_tests::gate(owner, 2, cx);
        let gesture = GestureId(2);
        let token = owner
            .link
            .microphone_gate()
            .arm(gesture)
            .expect("fresh hold");
        owner.link.track_input(gesture);
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session: 1,
            choice: owner.link.choice_revision(),
        };
        owner.voice_intent(
            Intent::Recognized {
                gesture,
                generation: Ok(token),
            },
            stamp,
            0,
            None,
            TalkHeld::default(),
            cx,
        );
        assert!(owner.voice.state.pending.is_none(), "refused at admission");
    });
    assert_eq!(
        shown(&owner, cx),
        (
            Some(Text::VoiceStatusUnknown),
            Some(strings.get(Text::VoiceStatusUnknown))
        ),
        "the lost connection's speech input is unknown"
    );
    assert_eq!(draft(&owner, cx), SENTINEL);

    let owner = recovered(cx);
    owner.update(cx, |owner, cx| owner.end_voice(Text::HoldUnavailable, cx));
    assert_eq!(
        shown(&owner, cx),
        (
            Some(Text::HoldUnavailable),
            Some(strings.get(Text::HoldUnavailable))
        )
    );
    assert_eq!(draft(&owner, cx), SENTINEL);
}

#[gpui::test]
fn esc_discards_the_copy_before_late_words_arrive(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        owner.escape(cx).expect("esc");
        assert!(owner.voice.state.recovery.is_none());
    });
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
    owner.read_with(cx, |owner, _| assert!(owner.voice.state.recovery.is_none()));

    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    thinking(&owner, cx);
    deliver(
        &owner,
        LinkEvent::RunState {
            generation: 1,
            run: RunId(1),
            thread: ThreadId(1),
            phase: Phase::Answering,
            error: None,
            origin: None,
        },
        cx,
    );
    owner.update(cx, |owner, cx| {
        assert!(
            owner.voice.state.pending.is_none(),
            "the answer ended the hold"
        );
        assert!(owner.voice.state.recovery.is_some(), "but not its copy");
        owner.escape(cx).expect("esc");
        assert!(owner.voice.state.recovery.is_none());
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
}

#[gpui::test]
fn a_captured_answer_escape_discards_its_words_before_loss(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    let native = native(&owner, cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    thinking(&owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        native(Event::Escape, after(at, 0));
    });
    deliver(&owner, run_state(Phase::Answering), cx);
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.state.pending.is_none());
        assert!(owner.voice.state.recovery.is_some());
    });
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(
            owner.voice.state.recovery.is_none(),
            "the captured stop spent its copy"
        );
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
    assert_eq!(lost(&owner, cx), None);
}

#[gpui::test]
fn an_old_receipt_keeps_a_newer_hold_s_words_recoverable(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    owner.update(cx, crate::presence::Presence::cancel_voice);
    cx.run_until_parked();
    owner.update(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
        assert!(!owner.link.voice_stop_pending());
    });
    hold(&owner, 2, cx);
    deliver(&owner, words(1, 2, SENTINEL, 40), cx);
    owner.update(cx, |owner, cx| {
        assert_eq!(owner.voice.state.ended, Some((GestureId(1), None)));
        owner
            .native_escape(Some(GestureId(1)), None, cx)
            .expect("old press");
        let copy = owner.voice.state.recovery.as_ref().expect("hold 2's copy");
        assert_eq!(copy.gesture, GestureId(2));
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), SENTINEL);
    assert_eq!(lost(&owner, cx), Some(Text::HoldLostRestored));
}

#[gpui::test]
fn an_occupied_composer_keeps_its_draft_and_gets_the_plain_notice(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    set_draft(&owner, "typed draft", cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "typed draft");
    assert_eq!(lost(&owner, cx), Some(Text::HoldLost));
    owner.read_with(cx, |owner, _| assert!(owner.voice.state.recovery.is_none()));
}

#[gpui::test]
fn a_hidden_composer_takes_the_words_without_showing(cx: &mut TestAppContext) {
    for (picture, notice) in [
        (false, Text::HoldLostRestored),
        (true, Text::HoldLostRestoredPictured),
    ] {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        owner.update(cx, |owner, _| {
            owner.voice.state.recovery.as_mut().expect("slot").picture = picture;
        });
        deliver(&owner, words(1, 1, SENTINEL, 40), cx);
        deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(draft(&owner, cx), SENTINEL);
        assert_eq!(lost(&owner, cx), Some(notice));
        owner.read_with(cx, |owner, _| {
            assert!(!owner.text.as_ref().expect("typing fixture").typing.visible);
        });
    }
    let strings = Strings::english();
    assert_eq!(
        strings.get(Text::HoldLostRestoredPictured),
        "Not sent. Your words are in the box. The picture wasn't kept."
    );
    assert_eq!(
        strings.get(Text::HoldLostPictured),
        "Not sent. Hold to try again. The picture wasn't kept."
    );
}

#[gpui::test]
fn words_go_back_only_to_the_holds_own_thread_and_selection(cx: &mut TestAppContext) {
    for change in 0..3 {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        owner.update(cx, |owner, _| {
            let slot = owner.voice.state.recovery.as_mut().expect("slot");
            assert_eq!(slot.target.0, Some(ThreadId(1)), "the main thread");
            match change {
                0 => slot.target.0 = Some(ThreadId(2)),
                1 => slot.target.1 += 1,
                _ => slot.target.0 = None,
            }
        });
        deliver(&owner, words(1, 1, SENTINEL, 40), cx);
        deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(draft(&owner, cx), "", "change {change}");
        assert_eq!(lost(&owner, cx), Some(Text::HoldLost), "change {change}");
    }
}

#[gpui::test]
fn with_no_composer_yet_the_plain_notice_shows(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    owner.update(cx, |owner, _| owner.text = None);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(lost(&owner, cx), Some(Text::HoldLost));
    owner.read_with(cx, |owner, _| assert!(owner.voice.state.recovery.is_none()));
}

#[gpui::test]
fn held_words_are_kept_up_to_their_bound_and_never_printed(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    let over = "w".repeat(MAX_TRANSCRIPT_BYTES + 1);
    let before = granted(&owner, cx);
    deliver(&owner, words(1, 1, &over, 16_400), cx);
    assert_eq!(granted(&owner, cx), before + 16_400);
    owner.read_with(cx, |owner, _| {
        assert!(
            owner
                .voice
                .state
                .recovery
                .as_ref()
                .expect("slot")
                .words
                .is_none()
        );
    });
    let exact = format!(
        "{SENTINEL}{}",
        "w".repeat(MAX_TRANSCRIPT_BYTES - SENTINEL.len())
    );
    let event = words(1, 1, &exact, 16_400);
    for printed in [
        format!("{event:?}"),
        format!("{:?}", Batch::Plain(vec![words(1, 1, &exact, 1)])),
    ] {
        assert_redacted(&printed);
    }
    deliver(&owner, event, cx);
    owner.read_with(cx, |owner, _| {
        let slot = owner.voice.state.recovery.as_ref().expect("slot");
        assert_eq!(slot.words.as_deref(), Some(exact.as_str()));
        assert_redacted(&format!("{slot:?}"));
    });
}
