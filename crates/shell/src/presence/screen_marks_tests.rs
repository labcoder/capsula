//! What the presence tells the screen watch about a hold that sends the screen, read as one
//! ordered sequence: what the sources were told and what was written to the brain.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::TestAppContext;
use kumochi_protocol::message::{
    ControlToBrain, DataToShell, GestureId, HeldScreen, HoldStatus, Phase, RunId, RunOrigin,
    SceneHeader, SceneId, ScreenClass, ScreenHolds, ScreenOff, ThreadId,
};
use kumochi_scene::{
    Record, deferred::Deferred, parse_screen_draw_record, record::ScreenDisplay, screen,
    text::Bounded,
};
use kumochi_shell_core::platform::screen_watch::{Changed, Sources};

use super::super::Presence;
use super::super::overlay_visibility::Visibility;
use super::super::screen_hold_tests::{
    DOWN, Ready, detached, line, listening, luna, on, receive, recognize, release, release_holding,
    screen_hold, screen_ready, screen_release,
};
use super::super::voice_wiring_tests::status;
use super::Source;
use super::tests::{after, listen, native, until};
use crate::input::{Edge, Event, Key, TalkHeld};
use crate::link::{LinkEvent, LinkState};
use crate::platform::input::{Button, input};
use crate::platform::{Deliver, PresenceEvent};
use crate::presence::kumo::{Motion, v2};
use crate::presence::pulling::{Grab, PullSpace};
use crate::strings::Text;
use kumochi_shell_core::presenter::fixture::select;

/// The hold's run. Nothing was named before its release.
const RUN: u64 = 7;
const MARKS: SceneId = SceneId(30);
const OTHER: SceneId = SceneId(31);
/// The test platform's one display, which the overlay covers: 1920 by 1080 at a scale of 2.
const DISPLAY: ScreenDisplay = ScreenDisplay {
    origin: [0, 0],
    size: [3840, 2160],
    dpi: 192,
};
/// The first state of the hold's run names the hold.
const HOLD: Option<RunOrigin> = Some(RunOrigin::Hold {
    gesture: GestureId(1),
});

/// One thing the sources or the brain were told.
#[derive(Debug, PartialEq)]
enum Told {
    /// The sources started, with left Ctrl and left Alt still down or not.
    Start(bool, bool),
    Stop,
    /// A control message written to the brain.
    Sent(ControlToBrain),
}

type Log = Rc<RefCell<Vec<Told>>>;
/// The bit the sources were last started with.
type Bit = Rc<RefCell<Option<Changed>>>;

/// Sources the test scripts. Each start and stop is logged after every control message
/// already written, so the log is one sequence in the order things happened.
struct Scripted {
    log: Log,
    bit: Bit,
    controls: async_channel::Receiver<ControlToBrain>,
    starts: bool,
}

impl Scripted {
    fn tell(&self, told: Told) {
        let mut log = self.log.borrow_mut();
        log.extend(std::iter::from_fn(|| self.controls.try_recv().ok()).map(Told::Sent));
        log.push(told);
    }
}

impl Sources for Scripted {
    fn start(&mut self, changed: Changed, held: TalkHeld) -> bool {
        self.tell(Told::Start(held.control, held.alt));
        self.bit.replace(Some(changed));
        self.starts
    }

    fn stop(&mut self) {
        self.tell(Told::Stop);
    }
}

struct Watching {
    ready: Ready,
    log: Log,
    bit: Bit,
}

/// A connected presence whose presenter mirrors scenes and has scripted sources, which start
/// or can't.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the scripted sources can start"
)]
fn watch(starts: bool, cx: &mut TestAppContext) -> Watching {
    let watching = bare(cx);
    let sources = Scripted {
        log: Rc::clone(&watching.log),
        bit: Rc::clone(&watching.bit),
        controls: watching.ready.controls.clone(),
        starts,
    };
    watching.ready.owner.update(cx, |owner, _| {
        owner.presenter.watch_sources(Box::new(sources));
    });
    watching
}

/// A connected presence whose presenter mirrors scenes and has no sources, as every shipped
/// build is.
fn bare(cx: &mut TestAppContext) -> Watching {
    let ready = screen_ready(Some(on(&luna())), cx);
    ready.owner.update(cx, |owner, _| {
        owner.presenter.connected(1);
        owner
            .presenter
            .described(1, &Bounded::default(), Instant::now())
            .expect("empty membership");
    });
    let (log, bit) = (Log::default(), Bit::default());
    Watching { ready, log, bit }
}

/// Everything told since the last read, in order.
fn told(watching: &Watching) -> Vec<Told> {
    let mut log = watching.log.take();
    log.extend(std::iter::from_fn(|| watching.ready.controls.try_recv().ok()).map(Told::Sent));
    log
}

/// What the sources alone were told since the last read.
fn sources(watching: &Watching) -> Vec<Told> {
    let mut all = told(watching);
    all.retain(|told| !matches!(told, Told::Sent(_)));
    all
}

/// Hold `id` is recognized and listening.
fn hold(watching: &Watching, id: u32, cx: &mut TestAppContext) {
    recognize(&watching.ready.owner, id, cx);
    listening(&watching.ready.owner, cx);
}

/// Hold 1 was released with no key left down, its release written and its speech sent.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the scripted sources can start"
)]
fn released(starts: bool, cx: &mut TestAppContext) -> Watching {
    let watching = watch(starts, cx);
    let_go(&watching, cx);
    watching
}

/// Holds and releases hold 1, as [`released`] does.
fn let_go(watching: &Watching, cx: &mut TestAppContext) {
    told(watching);
    hold(watching, 1, cx);
    release(&watching.ready.owner, 1, Some(DOWN), cx);
    assert_eq!(
        told(watching),
        [
            Told::Sent(screen_hold(1)),
            Told::Start(false, false),
            Told::Sent(screen_release(1, 1, DOWN, &[])),
        ]
    );
    watching.ready.owner.update(cx, |owner, _| {
        owner.voice.pending.as_mut().expect("the hold").source = Source::Sent;
    });
}

fn state(
    watching: &Watching,
    run: u64,
    phase: Phase,
    origin: Option<RunOrigin>,
    cx: &mut TestAppContext,
) {
    state_in(watching, run, 1, phase, origin, cx);
}

fn state_in(
    watching: &Watching,
    run: u64,
    thread: u64,
    phase: Phase,
    origin: Option<RunOrigin>,
    cx: &mut TestAppContext,
) {
    let event = LinkEvent::RunState {
        generation: 1,
        origin,
        run: RunId(run),
        thread: ThreadId(thread),
        phase,
        error: None,
    };
    receive(&watching.ready.owner, event, cx);
}

fn data(watching: &Watching, message: DataToShell, cx: &mut TestAppContext) {
    let event = LinkEvent::Data {
        generation: 1,
        message,
        bytes: 0,
    };
    receive(&watching.ready.owner, event, cx);
}

/// A piece of `run`'s answer, which puts that answer on screen.
fn text(watching: &Watching, run: u64, cx: &mut TestAppContext) {
    let message = DataToShell::AssistantText {
        run: RunId(run),
        delta: "an answer".into(),
    };
    data(watching, message, cx);
}

fn end(watching: &Watching, run: u64, cx: &mut TestAppContext) {
    data(watching, DataToShell::RunEnd { run: RunId(run) }, cx);
}

fn hold_screen(watching: &Watching, gesture: u32, outcome: HeldScreen, cx: &mut TestAppContext) {
    let event = LinkEvent::HoldScreen {
        generation: 1,
        gesture: GestureId(gesture),
        outcome,
    };
    receive(&watching.ready.owner, event, cx);
}

/// A screen scene of `run` begins and commits its first mark; its picture is of `display`.
fn mark(
    watching: &Watching,
    scene: SceneId,
    run: u64,
    display: ScreenDisplay,
    cx: &mut TestAppContext,
) {
    let begin = br#"{"begin":{"scene":"marks","surface":"screen","alt":""}}"#;
    let Ok(Record::Begin(mut begin)) = parse_screen_draw_record(begin) else {
        panic!("a screen begin record");
    };
    screen::stamp(&mut begin, [1280, 720], display).expect("stamped");
    let step =
        br#"{"step":{"id":"s","put":[{"highlight":{"id":"h","target":{"box":[1,2,3,4]}}}]}}"#;
    let Ok(Record::Step(step)) = parse_screen_draw_record(step) else {
        panic!("a screen step record");
    };
    for frame in [
        DataToShell::SceneBegin {
            scene,
            header: SceneHeader {
                begin,
                layout_version: 2,
            },
        },
        DataToShell::SceneStepCommit {
            run: RunId(run),
            scene,
            index: 0,
            step,
        },
    ] {
        let event = LinkEvent::SceneData {
            generation: 1,
            frame: Deferred::new(frame, 0),
            bytes: 0,
        };
        receive(&watching.ready.owner, event, cx);
    }
}

fn skipped(scene: SceneId) -> Told {
    Told::Sent(ControlToBrain::SceneSkipped { scene })
}

#[gpui::test]
fn the_watch_starts_before_the_release_is_written_and_an_unwritten_release_drops_it(
    cx: &mut TestAppContext,
) {
    // Left Ctrl is still down as Alt comes up: the sources start with that, and only then is
    // the release written.
    let watching = watch(true, cx);
    hold(&watching, 1, cx);
    assert_eq!(
        told(&watching),
        [Told::Sent(screen_hold(1))],
        "recognition starts nothing"
    );
    let held = TalkHeld {
        control: true,
        alt: false,
    };
    release_holding(&watching.ready.owner, 1, Some(DOWN), held, cx);
    assert_eq!(
        told(&watching),
        [
            Told::Start(true, false),
            Told::Sent(screen_release(1, 1, DOWN, &[])),
        ]
    );

    // A release that can't be written was still reported first, and is then dropped: the
    // sources stop, and nothing of that hold can name a run to the watch.
    let watching = watch(true, cx);
    hold(&watching, 1, cx);
    assert_eq!(told(&watching), [Told::Sent(screen_hold(1))]);
    assert!(watching.ready.controls.close());
    release(&watching.ready.owner, 1, Some(DOWN), cx);
    assert_eq!(told(&watching), [Told::Start(false, false), Told::Stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
}

#[gpui::test]
fn sources_that_can_t_start_show_no_marks_and_the_dock_says_so(cx: &mut TestAppContext) {
    for starts in [true, false] {
        let watching = released(starts, cx);
        state(&watching, RUN, Phase::Thinking, HOLD, cx);
        text(&watching, RUN, cx);
        assert_eq!(sources(&watching), []);
        mark(&watching, MARKS, RUN, DISPLAY, cx);
        if starts {
            assert_eq!(told(&watching), [], "the hold's own marks, on its display");
            assert_eq!(line(&watching.ready, cx), None);
        } else {
            assert_eq!(told(&watching), [skipped(MARKS)]);
            assert_eq!(line(&watching.ready, cx), Some((Text::MarksSkipped, None)));
        }
    }
}

#[gpui::test]
fn a_release_with_no_sources_does_none_of_the_watch_s_work(cx: &mut TestAppContext) {
    let watching = bare(cx);
    hold(&watching, 1, cx);
    release(&watching.ready.owner, 1, Some(DOWN), cx);
    assert_eq!(
        told(&watching),
        [
            Told::Sent(screen_hold(1)),
            Told::Sent(screen_release(1, 1, DOWN, &[])),
        ]
    );
    assert_eq!(watchdog(&watching, cx), None, "no limit to keep");
    watching.ready.owner.update(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
        owner.voice.pending.as_mut().expect("the hold").source = Source::Sent;
    });
    // A screen scene that arrives all the same finds no watch and is skipped.
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    text(&watching, RUN, cx);
    mark(&watching, MARKS, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(MARKS)]);
    assert_eq!(line(&watching.ready, cx), None);
}

#[gpui::test]
fn a_scene_that_comes_before_the_hold_s_run_is_named_is_never_the_hold_s(cx: &mut TestAppContext) {
    // An earlier hold's run had a screen scene queued, which arrives under this release
    // before either hold's first state: nothing of it shows, and the watch goes on.
    let watching = released(true, cx);
    mark(&watching, OTHER, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(OTHER)]);
    // The earlier hold's state names nothing. This hold's own run still shows its marks.
    let another = Some(RunOrigin::Hold {
        gesture: GestureId(9),
    });
    state(&watching, RUN, Phase::Thinking, another, cx);
    state(&watching, RUN + 1, Phase::Thinking, HOLD, cx);
    let stop = ControlToBrain::Stop {
        run: RunId(RUN),
        revealed: Vec::new(),
    };
    assert_eq!(told(&watching), [Told::Sent(stop)]);
    mark(&watching, MARKS, RUN + 1, DISPLAY, cx);
    assert_eq!(told(&watching), [], "the hold's own marks");
}

#[gpui::test]
fn the_dock_says_marks_were_skipped_only_beside_their_own_answer(cx: &mut TestAppContext) {
    let watching = watch(false, cx);
    // An answer from before the hold is on screen.
    state(&watching, 3, Phase::Answering, None, cx);
    text(&watching, 3, cx);
    state(&watching, 3, Phase::Done, None, cx);
    end(&watching, 3, cx);
    let_go(&watching, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    mark(&watching, MARKS, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(MARKS)]);
    assert_eq!(
        line(&watching.ready, cx),
        None,
        "the answer on screen is the one before"
    );
    text(&watching, RUN, cx);
    assert_eq!(line(&watching.ready, cx), Some((Text::MarksSkipped, None)));
}

#[gpui::test]
fn the_native_callback_hands_over_the_talk_key_still_down(cx: &mut TestAppContext) {
    let watching = watch(true, cx);
    let deliver = native(&watching.ready.owner, cx);
    let at = Instant::now();
    let finish = listen(&watching.ready.owner, &deliver, 1, at, cx);
    assert_eq!(sources(&watching), []);
    // Alt comes up first, which ends the hold with Ctrl still down.
    let key = Key::LeftAlt;
    let edge = Edge::Up;
    deliver(Event::Key { key, edge }, after(at, 900));
    cx.run_until_parked();
    assert_eq!(sources(&watching), [Told::Start(true, false)]);
    finish.send(()).expect("the fake microphone ends");
}

#[gpui::test]
fn only_the_run_the_hold_named_marks_the_screen(cx: &mut TestAppContext) {
    let watching = released(true, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    assert_eq!(sources(&watching), [], "still listening");
    // A newer run that isn't the hold's.
    mark(&watching, OTHER, RUN + 1, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(OTHER)]);
    mark(&watching, MARKS, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), []);

    // A run whose first state names another hold names nothing to the watch. The watched
    // hold's own run still does, and from then on only it marks the screen.
    let watching = released(true, cx);
    let another = Some(RunOrigin::Hold {
        gesture: GestureId(9),
    });
    state(&watching, RUN, Phase::Thinking, another, cx);
    state(&watching, RUN + 1, Phase::Thinking, HOLD, cx);
    let stop = ControlToBrain::Stop {
        run: RunId(RUN),
        revealed: Vec::new(),
    };
    assert_eq!(
        told(&watching),
        [Told::Sent(stop)],
        "the stray run is stopped"
    );
    mark(&watching, OTHER, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(OTHER)]);
    mark(&watching, MARKS, RUN + 1, DISPLAY, cx);
    assert_eq!(told(&watching), []);
}

#[gpui::test]
fn the_watch_ends_once_both_endings_of_the_hold_s_run_are_in(cx: &mut TestAppContext) {
    for state_first in [true, false] {
        let watching = released(true, cx);
        state(&watching, RUN, Phase::Thinking, HOLD, cx);
        state(&watching, RUN, Phase::Answering, None, cx);
        if state_first {
            state(&watching, RUN, Phase::Done, None, cx);
        } else {
            end(&watching, RUN, cx);
        }
        assert_eq!(sources(&watching), [], "one ending is in");
        if state_first {
            end(&watching, RUN, cx);
        } else {
            state(&watching, RUN, Phase::Done, None, cx);
        }
        assert_eq!(sources(&watching), [Told::Stop]);
    }

    // Another run that ends before the hold's is named ends the watch too, and the hold's
    // marks are then skipped.
    let watching = released(true, cx);
    state(&watching, 3, Phase::Answering, None, cx);
    state(&watching, 3, Phase::Done, None, cx);
    assert_eq!(sources(&watching), []);
    end(&watching, 3, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    mark(&watching, MARKS, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(MARKS)]);
}

#[gpui::test]
fn a_hold_whose_screenshot_or_run_never_comes_drops_the_watch_once(cx: &mut TestAppContext) {
    let changed = HeldScreen::NotIncluded(ScreenClass::Changed);
    // The screenshot was attached; another hold's wasn't; then this hold's wasn't, twice.
    let watching = released(true, cx);
    hold_screen(&watching, 1, HeldScreen::Screen, cx);
    hold_screen(&watching, 9, changed, cx);
    assert_eq!(sources(&watching), []);
    hold_screen(&watching, 1, changed, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
    hold_screen(&watching, 1, changed, cx);
    assert_eq!(sources(&watching), []);

    // The brain's word comes after the run is named and answering.
    let watching = released(true, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    assert_eq!(sources(&watching), []);
    hold_screen(&watching, 1, changed, cx);
    assert_eq!(sources(&watching), [Told::Stop]);

    // The hold heard nothing, so no run follows.
    let watching = released(true, cx);
    status(&watching.ready.owner, 1, 1, HoldStatus::Empty, cx);
    assert_eq!(sources(&watching), [Told::Stop]);

    // The run's thread is being deleted when its first state names it.
    let watching = released(true, cx);
    watching.ready.owner.update(cx, |owner, _| {
        assert!(owner.runs.confirm_delete(ThreadId(9)));
    });
    state_in(&watching, RUN, 9, Phase::Thinking, HOLD, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
    assert_eq!(watchdog(&watching, cx), None);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
}

#[gpui::test]
fn esc_the_next_hold_and_the_setting_end_the_watch(cx: &mut TestAppContext) {
    // Esc before the run: one stop, though the press also cancels the hold.
    let watching = released(true, cx);
    watching
        .ready
        .owner
        .update(cx, Presence::escape)
        .expect("escape");
    assert_eq!(sources(&watching), [Told::Stop]);

    // Esc while the answer runs: the watch ends, and the answer is stopped once.
    let watching = released(true, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    watching
        .ready
        .owner
        .update(cx, Presence::escape)
        .expect("escape");
    let stop = ControlToBrain::Stop {
        run: RunId(RUN),
        revealed: Vec::new(),
    };
    assert_eq!(told(&watching), [Told::Stop, Told::Sent(stop)]);

    // The next hold is recognized while the answer runs: the watch has ended before that
    // hold can release anything.
    let watching = released(true, cx);
    // The fixture never opened the microphone, so the gate is given back by hand.
    watching.ready.owner.read_with(cx, |owner, _| {
        let hold = owner.voice.pending.as_ref().expect("the hold");
        hold.token.discard_unstarted().expect("never started");
    });
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    assert_eq!(told(&watching), []);
    recognize(&watching.ready.owner, 2, cx);
    assert_eq!(told(&watching), [Told::Sent(screen_hold(2)), Told::Stop]);

    // The box is unchecked: the watch ends, and the hold already released shows no marks.
    let watching = released(true, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        owner.save_show_screen(false, cx);
    });
    assert_eq!(sources(&watching), [Told::Stop]);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    mark(&watching, MARKS, RUN, DISPLAY, cx);
    assert_eq!(told(&watching), [skipped(MARKS)]);

    // The brain says the setting is off.
    let watching = released(true, cx);
    let event = LinkEvent::ScreenHolds {
        generation: 1,
        state: ScreenHolds::Off(ScreenOff::TurnedOff),
    };
    receive(&watching.ready.owner, event, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
}

#[gpui::test]
fn a_release_that_shows_no_screen_starts_nothing(cx: &mut TestAppContext) {
    // The pointer couldn't be read, so the hold goes on voice only.
    let watching = watch(true, cx);
    hold(&watching, 1, cx);
    release(&watching.ready.owner, 1, None, cx);
    assert_eq!(told(&watching), [Told::Sent(screen_hold(1))]);
}

/// When the scene watchdog next checks, if it is armed.
fn watchdog(watching: &Watching, cx: &mut TestAppContext) -> Option<Instant> {
    watching
        .ready
        .owner
        .read_with(cx, |owner, _| owner.scene_deadline)
}

#[gpui::test]
fn the_scene_watchdog_keeps_the_watch_s_limit_and_lets_it_go(cx: &mut TestAppContext) {
    // A released hold whose run isn't named: the watch ends by itself after 30 seconds.
    let before = Instant::now();
    let watching = released(true, cx);
    let limit = watchdog(&watching, cx).expect("armed at the release");
    let unnamed = Duration::from_secs(30);
    assert!(limit >= before + unnamed && limit <= Instant::now() + unnamed);
    // Once the run is named the watch lasts as long as the answer.
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    assert_eq!(watchdog(&watching, cx), None);

    // Another run's end is the watch's end, and nothing is left to time.
    let watching = released(true, cx);
    state(&watching, 3, Phase::Answering, None, cx);
    state(&watching, 3, Phase::Done, None, cx);
    assert!(watchdog(&watching, cx).is_some());
    end(&watching, 3, cx);
    assert_eq!(watchdog(&watching, cx), None);

    // A dropped watch has no limit either.
    let watching = released(true, cx);
    hold_screen(
        &watching,
        1,
        HeldScreen::NotIncluded(ScreenClass::Changed),
        cx,
    );
    assert_eq!(watchdog(&watching, cx), None);
}

#[gpui::test]
fn a_lost_session_ends_the_watch_once(cx: &mut TestAppContext) {
    let watching = released(true, cx);
    let lost = LinkEvent::State(LinkState::Reconnecting);
    receive(&watching.ready.owner, lost, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });

    // The run is answering, so the hold is no longer pending: the loss still forgets it.
    let watching = released(true, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    let lost = LinkEvent::State(LinkState::Reconnecting);
    receive(&watching.ready.owner, lost, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
}

/// Hold 1's run is answering and its marks are up, with the sources listening.
fn marks_up(cx: &mut TestAppContext) -> Watching {
    marked(true, cx)
}

/// Hold 1's marks are up, with the sources listening. Its run is answering, or its first mark
/// overtook that state on the other pipe, so the hold is still pending.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the run's Answering state came before the mark"
)]
fn marked(answering: bool, cx: &mut TestAppContext) -> Watching {
    let watching = released(true, cx);
    mark_up(&watching, answering, cx);
    watching
}

/// The released hold 1's run is named and its first mark comes, after its `Answering` or
/// before it.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the run's Answering state came before the mark"
)]
fn mark_up(watching: &Watching, answering: bool, cx: &mut TestAppContext) {
    let hidden = detached(&watching.ready, cx);
    watching.ready.owner.update(cx, |owner, _| {
        owner.overlay = Visibility::Hidden(hidden);
    });
    state(watching, RUN, Phase::Thinking, HOLD, cx);
    if answering {
        state(watching, RUN, Phase::Answering, None, cx);
    }
    text(watching, RUN, cx);
    mark(watching, MARKS, RUN, DISPLAY, cx);
    cx.run_until_parked();
    let (run, scene, index) = (RunId(RUN), MARKS, 0);
    assert_eq!(
        told(watching),
        [
            Told::Sent(ControlToBrain::StepReady { run, scene, index }),
            Told::Sent(ControlToBrain::StepShown { run, scene, index }),
        ],
        "the marks are up and the sources listen"
    );
}

/// What one press of Esc tells while a running answer's marks are up: the sources stop, the
/// marks are cleared and the answer is stopped, once each.
fn taken() -> [Told; 3] {
    [
        Told::Stop,
        Told::Sent(ControlToBrain::ClearDrawing {
            scene_ids: vec![MARKS],
        }),
        Told::Sent(ControlToBrain::Stop {
            run: RunId(RUN),
            revealed: Vec::new(),
        }),
    ]
}

fn escape(watching: &Watching, cx: &mut TestAppContext) {
    watching
        .ready
        .owner
        .update(cx, Presence::escape)
        .expect("escape");
    cx.run_until_parked();
}

/// Kumo is grabbed out of the peek, which tells the brain nothing.
fn pull(watching: &Watching, cx: &mut TestAppContext) {
    let kumo = watching.ready.owner.update(cx, |owner, cx| {
        owner.follow_kumo(cx);
        owner.kumo.clone()
    });
    let space = PullSpace {
        area: [0, 0, 1280, 1024],
        scale: 1.0,
    };
    let grab = Grab::new(
        space,
        [612, 1000, 56, 3],
        [608, 990, 64, 24],
        v2(640.0, 920.0),
        Motion::Reduced,
    );
    let seq = input(|input| input.press(|| input.pressed())).expect("a forwarded press");
    kumo.update(cx, |kumo, cx| {
        kumo.grab(Button::Left, seq, [560, 860], [80.0, 60.0], grab, cx);
    });
    cx.run_until_parked();
    watching.ready.owner.read_with(cx, |owner, cx| {
        assert!(owner.kumo.read(cx).pull_live());
        assert!(owner.activity.pulling);
    });
    assert_eq!(told(watching), []);
}

#[gpui::test]
fn one_esc_takes_a_running_answer_s_marks_and_stops_it_whatever_else_it_does(
    cx: &mut TestAppContext,
) {
    // Nothing else is going on.
    let watching = marks_up(cx);
    escape(&watching, cx);
    assert_eq!(told(&watching), taken());

    // Kumo is being pulled out of the peek: the press cancels the pull as well.
    let watching = marks_up(cx);
    pull(&watching, cx);
    escape(&watching, cx);
    assert_eq!(told(&watching), taken());
    watching.ready.owner.read_with(cx, |owner, cx| {
        assert!(!owner.kumo.read(cx).pull_live(), "the pull was cancelled");
    });

    // A selection is live: the press ends it as well.
    let watching = marks_up(cx);
    watching.ready.owner.update(cx, |owner, _| {
        select(&mut owner.presenter.input, GestureId(5));
    });
    escape(&watching, cx);
    assert_eq!(told(&watching), taken());
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.input.selection(), None);
    });
}

/// The hold's cancel, which stops its run in the brain.
fn cancel() -> Told {
    Told::Sent(ControlToBrain::GestureCancel {
        gesture: GestureId(1),
    })
}

/// The sources stop and the marks are cleared, once each.
fn gone() -> [Told; 2] {
    [
        Told::Stop,
        Told::Sent(ControlToBrain::ClearDrawing {
            scene_ids: vec![MARKS],
        }),
    ]
}

/// The pending hold is gone and its run is fenced off.
fn ended(watching: &Watching, cx: &mut TestAppContext) {
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.runs.stoppable(), None);
    });
}

// The three tests below pin the hold's cancel as it is told today: twice for one press, once
// where the hold's input is revoked and once from the runs' hold.

#[gpui::test]
fn the_hotkey_s_esc_takes_a_pending_hold_s_marks_and_cancels_it(cx: &mut TestAppContext) {
    let watching = marked(false, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        assert!(owner.voice_working(), "the hold is still pending");
        // The hotkey's reducer revokes the hold's input before the press is applied.
        let cancelled = owner.link.cancel_input_gesture();
        assert_eq!(cancelled, Some(GestureId(1)));
        owner.native_escape(cancelled, cx).expect("escape");
    });
    cx.run_until_parked();
    let [stop, clear] = gone();
    assert_eq!(told(&watching), [cancel(), stop, clear, cancel()]);
    ended(&watching, cx);
}

/// Hold 1 was made and let go through the native callback, so the callback's reducer still
/// names it, and its first mark overtook its run's `Answering`: the marks are up while the
/// hold is pending.
fn natively_marked(cx: &mut TestAppContext) -> (Watching, Deliver, Instant) {
    let watching = watch(true, cx);
    let owner = watching.ready.owner.clone();
    let deliver = native(&owner, cx);
    let at = Instant::now();
    let finish = listen(&owner, &deliver, 1, at, cx);
    for key in [Key::LeftAlt, Key::LeftControl] {
        let edge = Edge::Up;
        deliver(Event::Key { key, edge }, after(at, 900));
    }
    finish.send(()).expect("the fake microphone ends");
    until(cx, |cx| {
        owner.read_with(cx, |owner, _| {
            let hold = owner.voice.pending.as_ref();
            hold.is_some_and(|hold| matches!(hold.source, Source::Sent))
        })
    });
    told(&watching);
    mark_up(&watching, false, cx);
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice_working(), "the hold is still pending");
        assert!(owner.text_open());
    });
    (watching, deliver, at)
}

/// The press took the marks and cancelled the hold, and did nothing more: the panel stays
/// open and the answer stays in it.
fn only_taken(watching: &Watching, cx: &mut TestAppContext) {
    ended(watching, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.marks_shown(), None);
        assert!(owner.text_open());
        assert_eq!(owner.runs.shown().map(|shown| shown.run), Some(RunId(RUN)));
    });
}

#[gpui::test]
fn esc_through_the_hotkey_s_reducer_takes_a_pending_hold_s_marks_and_cancels_it(
    cx: &mut TestAppContext,
) {
    // Kumochi is in front, so the sources don't count the key: the reducer's own cancel ends
    // the hold first, and the press then takes the marks.
    let (watching, deliver, at) = natively_marked(cx);
    let (key, edge) = (Key::Other(1), Edge::Down);
    deliver(Event::Key { key, edge }, after(at, 2000));
    deliver(Event::Escape, after(at, 2000));
    cx.run_until_parked();
    let [stop, clear] = gone();
    assert_eq!(told(&watching), [cancel(), cancel(), stop, clear]);
    only_taken(&watching, cx);
}

#[gpui::test]
fn esc_in_another_app_takes_a_pending_hold_s_marks_and_cancels_it(cx: &mut TestAppContext) {
    // Another app is in front, so the sources count the key as well. Their wake is handled
    // before the hotkey's press or after it, and the marks go once either way.
    for woke_first in [true, false] {
        let (watching, deliver, at) = natively_marked(cx);
        let wake = |cx: &mut TestAppContext| {
            watching.ready.owner.update(cx, |owner, cx| {
                owner.on_event(PresenceEvent::ScreenWoke, cx).expect("a wake");
            });
        };
        let (key, edge) = (Key::Other(1), Edge::Down);
        deliver(Event::Key { key, edge }, after(at, 2000));
        let heard = watching.bit.borrow().as_ref().expect("started sources").set();
        assert!(heard, "the key is the first change since the release");
        if woke_first {
            wake(cx);
        }
        deliver(Event::Escape, after(at, 2000));
        cx.run_until_parked();
        if !woke_first {
            wake(cx);
        }
        let [stop, clear] = gone();
        let expected = if woke_first {
            [cancel(), stop, clear, cancel()]
        } else {
            [cancel(), cancel(), stop, clear]
        };
        assert_eq!(told(&watching), expected);
        only_taken(&watching, cx);
    }
}

#[gpui::test]
fn esc_takes_a_pending_hold_s_marks_and_cancels_it(cx: &mut TestAppContext) {
    let watching = marked(false, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        assert!(owner.voice_working(), "the hold is still pending");
        owner.escape(cx).expect("escape");
    });
    cx.run_until_parked();
    let [stop, clear] = gone();
    assert_eq!(told(&watching), [stop, clear, cancel(), cancel()]);
    ended(&watching, cx);
}

#[gpui::test]
fn a_press_that_takes_an_ended_answer_s_marks_stops_no_other_run(cx: &mut TestAppContext) {
    // The marks' answer has ended and a newer run works while Kumo is pulled: the press takes
    // the marks and cancels the pull, and the newer run goes on as it would with no marks up.
    let watching = marks_up(cx);
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    state(&watching, RUN + 1, Phase::Thinking, None, cx);
    let newer = Some(RunId(RUN + 1));
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), newer);
    });
    assert_eq!(told(&watching), []);
    pull(&watching, cx);
    escape(&watching, cx);
    assert_eq!(
        told(&watching),
        [
            Told::Stop,
            Told::Sent(ControlToBrain::ClearDrawing {
                scene_ids: vec![MARKS]
            }),
        ]
    );
    watching.ready.owner.read_with(cx, |owner, cx| {
        assert!(!owner.kumo.read(cx).pull_live(), "the pull was cancelled");
        assert_eq!(owner.runs.stoppable(), newer, "and the newer run goes on");
    });
}

#[gpui::test]
fn shown_marks_are_timed_from_their_answer_s_end_and_one_esc_clears_them(cx: &mut TestAppContext) {
    let before = Instant::now();
    let watching = marks_up(cx);

    // Both endings in: the marks get their minute, and the sources go on listening.
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    let limit = watchdog(&watching, cx).expect("armed at the answer's end");
    let kept = Duration::from_secs(60);
    assert!(limit >= before + kept && limit <= Instant::now() + kept);
    assert_eq!(told(&watching), []);

    // One press of Esc takes them.
    watching
        .ready
        .owner
        .update(cx, Presence::escape)
        .expect("escape");
    assert_eq!(
        told(&watching),
        [
            Told::Stop,
            Told::Sent(ControlToBrain::ClearDrawing {
                scene_ids: vec![MARKS]
            }),
        ]
    );
}
