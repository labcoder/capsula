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

#[gpui::test]
fn the_draw_replay_prompt_waits_for_the_queued_registration(cx: &mut TestAppContext) {
    let _area = work_area_fixture::set([0, 0, 2560, 1400]);
    let (id, hello, probe, _brain) = kumochi_brain::ipc::offline_text_peer("0 text Drawn.".into());
    let pipe = fixture::authenticated_primed(id, hello);
    let events = fixture::patient(pipe.events.clone());
    let owner = test_fixture::presence(cx);
    owner.update(cx, |owner, _| {
        owner.link = pipe.handle.clone();
        owner.draw_fixture = Some(PROMPT);
    });
    let session = fixture::connected(fixture::next(&events));
    apply(&owner, LinkEvent::Connected(session), cx);
    loop {
        let event = fixture::next(&events);
        let settings = matches!(event, LinkEvent::Settings { .. });
        apply(&owner, event, cx);
        if settings {
            break;
        }
    }
    // The brain hasn't acknowledged the audio registration, so a turn now is refused.
    owner.read_with(cx, |owner, _| assert_eq!(owner.draw_fixture, Some(PROMPT)));
    probe.release_ack();
    probe.release_run();
    probe.allow_data_credit();
    loop {
        let event = fixture::next(&events);
        let ready = matches!(event, LinkEvent::TextReady { .. });
        apply(&owner, event, cx);
        if ready {
            break;
        }
    }
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
