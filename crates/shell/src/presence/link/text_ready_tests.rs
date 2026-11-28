use super::Presence;
use crate::link::{Batch, LinkEvent, fixture};
use crate::platform::work_area_fixture;
use crate::presence::{run_fence_tests, test_fixture};
use gpui_kit::{App, Entity, TestAppContext};
use kumochi_protocol::message::{Phase, RunOrigin};
use std::sync::mpsc::Receiver;

const DRAFT: &str = "Hello there";

fn apply(owner: &Entity<Presence>, event: LinkEvent, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(vec![event]), cx)
            .expect("the link event applies");
    });
}

fn draft(owner: &Presence, cx: &App) -> String {
    owner
        .text
        .as_ref()
        .expect("the composer")
        .input
        .read(cx)
        .value()
        .to_string()
}

/// When the early Enter comes, relative to the pair's first Settings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pressed {
    BeforeSettings,
    AfterSettings,
}

/// Presses Enter on a queued pair before or after its first Settings, but before the brain
/// acknowledges its audio registration; returns the pair's `TextReady`, not yet applied.
fn early_enter(
    owner: &Entity<Presence>,
    pressed: Pressed,
    cx: &mut TestAppContext,
) -> (fixture::Authenticated, Receiver<LinkEvent>, LinkEvent) {
    let (id, hello, probe, _brain) = kumochi_brain::ipc::offline_text_peer("0 text Hi.".into());
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events.clone());
    owner.update(cx, |owner, _| {
        owner.link = pipe.handle.clone();
        // Onboarding would close the composer at the first Settings, and with it the hold.
        owner.onboarding_run = crate::onboarding::ThisRun::PutOff;
    });
    let session = fixture::connected(fixture::next(&events));
    apply(owner, LinkEvent::Connected(session), cx);
    let open = |cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| {
            owner.open_text(cx).expect("the composer opens");
        });
        run_fence_tests::submit_draft(owner, DRAFT, cx);
    };
    if pressed == Pressed::BeforeSettings {
        open(cx);
    }
    loop {
        let event = fixture::next(&events);
        let settings = matches!(event, LinkEvent::Settings { .. });
        apply(owner, event, cx);
        if settings {
            break;
        }
    }
    if pressed == Pressed::AfterSettings {
        open(cx);
    }
    owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner.refusal, None,
            "an idle pair opening to turns refuses nothing"
        );
        assert_eq!(draft(owner, cx), DRAFT, "the draft waits in the composer");
    });
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    loop {
        let event = fixture::next(&events);
        if matches!(event, LinkEvent::TextReady { .. }) {
            return (pipe, events, event);
        }
        apply(owner, event, cx);
    }
}

#[gpui::test]
fn an_enter_before_the_pair_admits_turns_is_sent_once_it_does(cx: &mut TestAppContext) {
    enter_is_sent_at_text_ready(Pressed::AfterSettings, cx);
}

#[gpui::test]
fn an_enter_before_the_first_settings_is_sent_once_the_pair_admits_turns(cx: &mut TestAppContext) {
    enter_is_sent_at_text_ready(Pressed::BeforeSettings, cx);
}

fn enter_is_sent_at_text_ready(pressed: Pressed, cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (_pipe, events, ready) = early_enter(&owner, pressed, cx);
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| {
        assert_eq!(draft(owner, cx), "", "the held Enter sends the draft");
    });
    loop {
        match fixture::next(&events) {
            LinkEvent::RunState {
                phase,
                error,
                origin,
                ..
            } => {
                assert_eq!(
                    (phase, error, origin),
                    (Phase::Thinking, None, Some(RunOrigin::Typed { turn: 1 })),
                    "the held Enter starts the first typed turn"
                );
                break;
            }
            LinkEvent::State(state) => panic!("the pair ended before the turn's run: {state:?}"),
            event => apply(&owner, event, cx),
        }
    }
}

#[gpui::test]
fn esc_drops_an_enter_still_waiting_for_the_pair(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (_pipe, _events, ready) = early_enter(&owner, Pressed::AfterSettings, cx);
    owner.update(cx, |owner, cx| {
        owner.dismiss_text(cx).expect("Esc closes the composer");
    });
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| assert_eq!(draft(owner, cx), DRAFT));
}

#[gpui::test]
fn quit_drops_an_enter_still_waiting_for_the_pair(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (pipe, _events, ready) = early_enter(&owner, Pressed::AfterSettings, cx);
    assert!(pipe.handle.begin_quit());
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| assert_eq!(draft(owner, cx), DRAFT));
}

#[gpui::test]
fn an_edited_draft_waits_for_another_enter(cx: &mut TestAppContext) {
    const EDITED: &str = "Something else";
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (_pipe, _events, ready) = early_enter(&owner, Pressed::AfterSettings, cx);
    owner.update(cx, |owner, cx| {
        let text = owner.text.as_ref().expect("the composer");
        text.window
            .update(cx, |_, window, cx| {
                text.input
                    .update(cx, |input, cx| input.set_value(EDITED, window, cx));
            })
            .expect("the composer window");
    });
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| assert_eq!(draft(owner, cx), EDITED));
}

#[gpui::test]
fn a_changed_destination_waits_for_another_enter(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (_pipe, _events, ready) = early_enter(&owner, Pressed::AfterSettings, cx);
    owner.update(cx, |owner, _| {
        assert!(
            owner.runs.select_thread(None),
            "the target moves to a new thread"
        );
    });
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| assert_eq!(draft(owner, cx), DRAFT));
}
