use super::Presence;
use crate::link::{Batch, LinkEvent, fixture};
use crate::platform::work_area_fixture;
use crate::presence::{run_fence_tests, test_fixture};
use gpui_kit::{App, Entity, TestAppContext};
use kumochi_protocol::message::Phase;
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

/// Presses Enter on a queued pair after its first Settings but before the brain acknowledges its
/// audio registration; returns the pair's `TextReady`, not yet applied.
fn early_enter(
    owner: &Entity<Presence>,
    cx: &mut TestAppContext,
) -> (fixture::Authenticated, Receiver<LinkEvent>, LinkEvent) {
    let (id, hello, probe, _brain) = kumochi_brain::ipc::offline_text_peer("0 text Hi.".into());
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events.clone());
    owner.update(cx, |owner, _| owner.link = pipe.handle.clone());
    let session = fixture::connected(fixture::next(&events));
    apply(owner, LinkEvent::Connected(session), cx);
    loop {
        let event = fixture::next(&events);
        let settings = matches!(event, LinkEvent::Settings { .. });
        apply(owner, event, cx);
        if settings {
            break;
        }
    }
    owner.update(cx, |owner, cx| {
        owner.open_text(cx).expect("the composer opens");
    });
    run_fence_tests::submit_draft(owner, DRAFT, cx);
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
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (_pipe, events, ready) = early_enter(&owner, cx);
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| assert_eq!(draft(owner, cx), ""));
    loop {
        match fixture::next(&events) {
            LinkEvent::RunState { phase, error, .. } => {
                assert_eq!((phase, error), (Phase::Thinking, None));
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
    let (_pipe, _events, ready) = early_enter(&owner, cx);
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
    let (pipe, _events, ready) = early_enter(&owner, cx);
    assert!(pipe.handle.begin_quit());
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, cx| assert_eq!(draft(owner, cx), DRAFT));
}
