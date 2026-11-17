use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::{DataToShell, Incarnation, RunId};

use super::super::PresenceEvent;
use super::super::voice_wiring_tests::{hold, ready, status};
use super::*;
use crate::link::{Batch, LinkEvent, LinkState, Session};

const SENTINEL: &str = "zqxsentinellabel held words";

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
        owner.voice.pending.as_mut().expect("a hold").source = Source::Sent;
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
    owner.read_with(cx, |owner, _| owner.voice.lost)
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
        assert!(owner.voice.recovery.is_some(), "Thinking keeps the copy");
        owner.text.as_ref().expect("typing fixture").typing.visible
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), SENTINEL);
    assert_eq!(lost(&owner, cx), Some(Text::HoldLostRestored));
    owner.read_with(cx, |owner, cx| {
        assert!(owner.voice.recovery.is_none());
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
fn the_stored_receipt_consumes_the_copy_and_a_later_loss_restores_nothing(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    thinking(&owner, cx);
    let before = granted(&owner, cx);
    deliver(&owner, stored(1, 1), cx);
    assert_eq!(granted(&owner, cx), before + 6);
    owner.read_with(cx, |owner, _| assert!(owner.voice.recovery.is_none()));
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
    assert_eq!(lost(&owner, cx), None);
}

#[gpui::test]
fn words_after_a_terminal_status_or_from_another_hold_are_refused_with_their_credit(
    cx: &mut TestAppContext,
) {
    for terminal in [
        HoldStatus::Cancelled,
        HoldStatus::Failed(HoldFailure::TimedOut),
    ] {
        let (owner, _data, _controls) = ready(cx);
        hold(&owner, 1, cx);
        status(&owner, 1, 1, terminal, cx);
        let before = granted(&owner, cx);
        deliver(&owner, words(1, 1, SENTINEL, 40), cx);
        assert_eq!(granted(&owner, cx), before + 40);
        owner.read_with(cx, |owner, _| assert!(owner.voice.recovery.is_none()));
        deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(draft(&owner, cx), "");
    }
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    deliver(&owner, words(0, 1, SENTINEL, 40), cx);
    deliver(&owner, words(1, 2, SENTINEL, 40), cx);
    deliver(&owner, stored(0, 1), cx);
    deliver(&owner, stored(1, 2), cx);
    owner.read_with(cx, |owner, _| {
        let slot = owner.voice.recovery.as_ref().expect("the live hold's slot");
        assert!(slot.words.is_none(), "another connection's or hold's words");
    });
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    deliver(&owner, words(1, 1, "a second copy", 30), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), SENTINEL);
}

#[gpui::test]
fn esc_discards_the_copy_before_late_words_arrive(cx: &mut TestAppContext) {
    let (owner, _data, _controls) = ready(cx);
    hold(&owner, 1, cx);
    owner.update(cx, |owner, cx| {
        owner.escape(cx).expect("esc");
        assert!(owner.voice.recovery.is_none());
    });
    deliver(&owner, words(1, 1, SENTINEL, 40), cx);
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
    owner.read_with(cx, |owner, _| assert!(owner.voice.recovery.is_none()));

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
        assert!(owner.voice.pending.is_none(), "the answer ended the hold");
        assert!(owner.voice.recovery.is_some(), "but not its copy");
        owner.escape(cx).expect("esc");
        assert!(owner.voice.recovery.is_none());
    });
    deliver(&owner, LinkEvent::State(LinkState::Reconnecting), cx);
    assert_eq!(draft(&owner, cx), "");
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
    owner.read_with(cx, |owner, _| assert!(owner.voice.recovery.is_none()));
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
            owner.voice.recovery.as_mut().expect("slot").picture = picture;
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
            let slot = owner.voice.recovery.as_mut().expect("slot");
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
    owner.read_with(cx, |owner, _| assert!(owner.voice.recovery.is_none()));
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
        assert!(owner.voice.recovery.as_ref().expect("slot").words.is_none());
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
        assert!(!printed.contains("zqxsentinellabel"));
        assert!(
            !printed
                .as_bytes()
                .windows(16)
                .any(|w| w == b"zqxsentinellabel")
        );
    }
    deliver(&owner, event, cx);
    owner.read_with(cx, |owner, _| {
        let slot = owner.voice.recovery.as_ref().expect("slot");
        assert_eq!(slot.words.as_deref(), Some(exact.as_str()));
        let printed = format!("{slot:?}");
        assert!(!printed.contains("zqxsentinellabel"));
        assert!(
            !printed
                .as_bytes()
                .windows(16)
                .any(|w| w == b"zqxsentinellabel")
        );
    });
}
