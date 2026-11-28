use super::Presence;
use crate::link::{Batch, LinkEvent, fixture};
use crate::platform::work_area_fixture;
use crate::presence::test_fixture;
use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::message::Phase;

const PROMPT: &str = "Draw the timing fixture.";

fn apply(owner: &Entity<Presence>, event: LinkEvent, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        owner
            .link_events(Batch::Plain(vec![event]), cx)
            .expect("the link event applies");
    });
}

/// Connects a queued pair with the prompt held, applies events through its first Settings and
/// releases the brain's acknowledgement; returns the pair's `TextReady`, not yet applied.
fn settled_pair(
    owner: &Entity<Presence>,
    cx: &mut TestAppContext,
) -> (
    fixture::Authenticated,
    std::sync::mpsc::Receiver<LinkEvent>,
    LinkEvent,
) {
    let (id, hello, probe, _brain) = kumochi_brain::ipc::offline_text_peer("0 text Drawn.".into());
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events.clone());
    owner.update(cx, |owner, _| {
        owner.link = pipe.handle.clone();
        owner.draw_fixture = Some(PROMPT);
    });
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
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.draw_fixture,
            Some(PROMPT),
            "before the acknowledgement a turn is refused, so the prompt waits"
        );
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
fn the_draw_replay_prompt_waits_for_the_queued_registration(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (_pipe, events, ready) = settled_pair(&owner, cx);
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, _| assert_eq!(owner.draw_fixture, None));
    loop {
        match fixture::next(&events) {
            LinkEvent::RunState { phase, error, .. } => {
                assert_eq!((phase, error), (Phase::Thinking, None));
                break;
            }
            LinkEvent::State(state) => panic!("the pair ended before the prompt's run: {state:?}"),
            event => apply(&owner, event, cx),
        }
    }
}

#[gpui::test]
fn a_quit_before_the_ready_event_applies_sends_no_draw_replay_prompt(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let owner = test_fixture::presence(cx);
    let (pipe, _events, ready) = settled_pair(&owner, cx);
    assert!(pipe.handle.begin_quit());
    apply(&owner, ready, cx);
    owner.read_with(cx, |owner, _| assert_eq!(owner.draw_fixture, Some(PROMPT)));
}
