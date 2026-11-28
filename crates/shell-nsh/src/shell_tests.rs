use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    DataToShell, Incarnation, RunId, SceneDescriptor, SceneDescriptorV1, SceneId,
};
use kumochi_scene::deferred::Deferred;
use kumochi_scene::text::Bounded;
use kumochi_shell_core::presenter::fixture::{commit_frame, header, rect};

use super::*;
use crate::platform::backstop::Limit;

const A: SceneId = SceneId(1);
const B: SceneId = SceneId(2);
/// Half the brain's 512 KiB data window.
const HALF_WINDOW: u32 = 256 * 1024;
/// Far longer than any test waits.
const NEVER: Duration = Duration::from_secs(600);

fn session(generation: u64) -> Session {
    Session::detached(generation, Incarnation(1))
}

/// A link whose scene refusals the test reads.
fn recording() -> (LinkHandle, Receiver<ControlToBrain>) {
    let mut link = LinkHandle::detached();
    let controls = link.record_controls();
    (link, controls)
}

fn controls(heard: &Receiver<ControlToBrain>) -> Vec<ControlToBrain> {
    std::iter::from_fn(|| heard.try_recv().ok()).collect()
}

fn data(generation: u64, bytes: u32) -> LinkEvent {
    LinkEvent::Data {
        generation,
        message: DataToShell::RunEnd { run: RunId(5) },
        bytes,
    }
}

fn scene(generation: u64, frame: DataToShell, bytes: u32) -> LinkEvent {
    LinkEvent::SceneData {
        generation,
        frame: Deferred::new(frame, 0),
        bytes,
    }
}

fn begin(generation: u64, scene_id: SceneId, bytes: u32) -> LinkEvent {
    scene(
        generation,
        DataToShell::SceneBegin {
            scene: scene_id,
            header: header(),
        },
        bytes,
    )
}

fn commit(generation: u64, scene_id: SceneId, index: u32, bytes: u32) -> LinkEvent {
    scene(generation, commit_frame(scene_id, index, rect("r")), bytes)
}

/// What each event asks, in order: a name per act, or "" for none.
fn applied(shell: &mut Shell, events: impl IntoIterator<Item = LinkEvent>) -> Vec<&'static str> {
    events
        .into_iter()
        .map(|event| name(shell.apply(event).as_ref()))
        .collect()
}

fn name(act: Option<&Act>) -> &'static str {
    match act {
        None => "",
        Some(Act::Opened) => "opened",
        Some(Act::Reconnecting) => "reconnecting",
        Some(Act::Quit) => "quit",
        Some(Act::End(..)) => "end",
    }
}

/// Runs the pump over `events` with the sender gone after them, so it ends without waiting.
fn pumped(link: Option<LinkHandle>, events: Vec<LinkEvent>) -> Vec<&'static str> {
    let (sender, receiver) = async_channel::unbounded();
    for event in events {
        sender.try_send(event).expect("the channel is unbounded");
    }
    drop(sender);
    let mut acts = Vec::new();
    {
        let mut running = pin!(pump(link, receiver, |act| acts.push(name(Some(&act)))));
        assert_eq!(
            running
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(()),
            "every event is queued and the channel closed, so the pump ends at once"
        );
    }
    acts
}

#[test]
fn killed_and_not_coming_back_reconnects_fails_and_quits_once() {
    let mut shell = Shell::new(LinkHandle::detached());
    assert_eq!(
        applied(
            &mut shell,
            [
                LinkEvent::Connected(session(1)),
                LinkEvent::State(LinkState::Reconnecting),
                LinkEvent::State(LinkState::Failed),
            ]
        ),
        ["opened", "reconnecting", "quit"]
    );
}

#[test]
fn an_unavailable_output_quits_at_once() {
    let mut shell = Shell::new(LinkHandle::detached());
    assert_eq!(
        applied(
            &mut shell,
            [
                LinkEvent::Connected(session(1)),
                LinkEvent::State(LinkState::OutputUnavailable),
            ]
        ),
        ["opened", "quit"]
    );
}

#[test]
fn killed_and_relaunched_reconnects_without_quitting() {
    let (first, second) = (session(1), session(2));
    let mut shell = Shell::new(LinkHandle::detached());
    assert_eq!(
        applied(
            &mut shell,
            [
                LinkEvent::Connected(first.clone()),
                LinkEvent::State(LinkState::Reconnecting),
                data(1, 7),
                LinkEvent::Connected(second.clone()),
                data(1, 10),
                data(2, 20),
            ]
        ),
        ["opened", "reconnecting", "", "opened", "", ""]
    );
    assert_eq!(
        first.granted(),
        0,
        "nothing is credited between the loss and the next connection, nor after"
    );
    assert_eq!(
        second.granted(),
        20,
        "the new connection's frame is credited"
    );
}

#[test]
fn only_an_open_connection_disarms_the_start_limit() {
    let closed = session(1);
    closed.close();
    let mut shell = Shell::new(LinkHandle::detached());
    assert_eq!(
        applied(
            &mut shell,
            [
                LinkEvent::Connected(closed),
                LinkEvent::Connected(session(2))
            ]
        ),
        ["", "opened"]
    );
}

#[test]
fn every_data_frame_is_credited_once_through_the_refuser() {
    let (link, heard) = recording();
    let open = session(1);
    let mut shell = Shell::new(link);
    applied(
        &mut shell,
        [
            LinkEvent::Connected(open.clone()),
            data(1, 100),
            begin(1, A, 200),
            commit(1, A, 0, 300),
            scene(1, DataToShell::ScenesRemoved { scenes: vec![A] }, 400),
            data(1, 500),
        ],
    );
    assert_eq!(open.granted(), 1500, "each frame's bytes, once");
    assert_eq!(controls(&heard), [ControlToBrain::SceneFailed { scene: A }]);
}

#[test]
fn a_scene_a_call_draws_on_is_refused_once() {
    let (link, heard) = recording();
    let mut shell = Shell::new(link);
    let asked = applied(
        &mut shell,
        [
            LinkEvent::Connected(session(1)),
            begin(1, A, 10),
            commit(1, A, 0, 10),
            commit(1, A, 1, 10),
            begin(1, B, 10),
        ],
    );
    assert_eq!(asked, ["opened", "", "", "", ""]);
    assert_eq!(
        controls(&heard),
        [
            ControlToBrain::SceneFailed { scene: A },
            ControlToBrain::SceneFailed { scene: B }
        ]
    );
}

#[test]
fn a_delayed_control_list_owes_no_credit() {
    let (link, heard) = recording();
    let open = session(1);
    let mut shell = Shell::new(link);
    let asked = applied(
        &mut shell,
        [
            LinkEvent::Connected(open.clone()),
            data(1, 100),
            LinkEvent::ScenesV1 {
                generation: 1,
                scenes: Bounded::new(vec![
                    SceneDescriptorV1::new(A, 3, 3, 3).expect("a valid descriptor"),
                ])
                .expect("one scene"),
            },
            LinkEvent::Scenes(vec![SceneDescriptor {
                scene: B,
                revision: 2,
                committed: 2,
            }]),
        ],
    );
    assert_eq!(asked, ["opened", "", "", ""]);
    assert_eq!(open.granted(), 100, "only the data frame earns credit");
    assert_eq!(controls(&heard), [], "a listed scene is left alone");
}

#[test]
fn frames_past_one_window_are_all_granted_on_their_session() {
    let (link, heard) = recording();
    let open = session(1);
    let mut shell = Shell::new(link);
    applied(
        &mut shell,
        [
            LinkEvent::Connected(open.clone()),
            begin(1, A, HALF_WINDOW),
            commit(1, A, 0, HALF_WINDOW),
            commit(1, A, 1, HALF_WINDOW),
        ],
    );
    assert_eq!(
        open.granted(),
        3 * HALF_WINDOW,
        "past one window, all of it"
    );
    assert_eq!(controls(&heard), [ControlToBrain::SceneFailed { scene: A }]);
}

#[test]
fn a_refused_refusal_seals_the_session_and_returns_its_end() {
    // No control recording: the detached link's mailbox refuses every refusal.
    let open = session(1);
    let mut shell = Shell::new(LinkHandle::detached());
    assert_eq!(
        name(shell.apply(LinkEvent::Connected(open.clone())).as_ref()),
        "opened"
    );
    let Some(Act::End(_, ended)) = shell.apply(begin(1, A, 50)) else {
        panic!("the refused send returns the session's end");
    };
    assert_eq!(ended.generation(), 1, "it ends the frame's own connection");
    assert!(!open.open(), "sealed here, before the end runs elsewhere");
    assert_eq!(
        open.granted(),
        50,
        "the frame was credited before the refusal"
    );
    assert_eq!(
        applied(&mut shell, [begin(1, B, 70), data(1, 70)]),
        ["", ""],
        "nothing more is refused or ended on it"
    );
}

#[test]
fn the_event_channel_ending_asks_for_quit() {
    assert_eq!(pumped(Some(LinkHandle::detached()), Vec::new()), ["quit"]);
    assert_eq!(
        pumped(
            Some(LinkHandle::detached()),
            vec![LinkEvent::Connected(session(1))]
        ),
        ["opened", "quit"]
    );
}

#[test]
fn a_link_that_never_started_asks_for_quit() {
    assert_eq!(pumped(None, Vec::new()), ["quit"]);
}

#[test]
fn quit_is_asked_once_when_two_causes_arrive() {
    assert_eq!(
        pumped(
            Some(LinkHandle::detached()),
            vec![
                LinkEvent::Connected(session(1)),
                LinkEvent::State(LinkState::OutputUnavailable),
                LinkEvent::State(LinkState::Failed),
            ]
        ),
        ["opened", "quit"],
        "two terminal states and the channel's end ask once"
    );
}

#[test]
fn a_failure_stays_failed_when_valid_events_follow() {
    let (link, heard) = recording();
    let later = session(2);
    let mut shell = Shell::new(link);
    assert_eq!(
        applied(
            &mut shell,
            [
                LinkEvent::State(LinkState::Failed),
                LinkEvent::Connected(later.clone()),
                data(2, 10),
                begin(2, A, 10),
            ]
        ),
        ["quit", "", "", ""]
    );
    assert_eq!(later.granted(), 0, "nothing after the failure is taken");
    assert_eq!(controls(&heard), []);
}

#[test]
fn a_quit_asked_twice_runs_once_and_arms_the_grace() {
    let (fired, heard) = std::sync::mpsc::channel();
    let backstop = Backstop::arm(
        Limit::Start(Instant::now()),
        Duration::from_millis(50),
        NEVER,
        move || fired.send(()).expect("the test is listening"),
    )
    .expect("the backstop starts");
    let link = LinkHandle::detached();
    let owner = Quit::new(backstop, Some(link.clone()));
    assert!(
        matches!(owner.begin(), Some(Begun::Finish(_))),
        "the first ask finishes the link"
    );
    assert!(link.is_quitting(), "the first ask marks Quit on the link");
    assert!(owner.begin().is_none(), "a second ask does nothing");
    assert_eq!(
        heard.recv_timeout(Duration::from_secs(30)),
        Ok(()),
        "the quit armed the grace, long before the start limit"
    );
}

#[test]
fn a_quit_with_no_link_quits_at_once() {
    let backstop = Backstop::arm(Limit::Start(Instant::now()), NEVER, NEVER, || {})
        .expect("the backstop starts");
    assert!(matches!(
        Quit::new(backstop, None).begin(),
        Some(Begun::Now)
    ));
}
