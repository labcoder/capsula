//! What the presence tells the screen watch about a hold that sends the screen, read as one
//! ordered sequence: what the sources were told and what was written to the brain.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::TestAppContext;
use kumochi_protocol::message::{
    CaptureFor, ControlToBrain, DataToShell, GestureId, HeldScreen, HoldStatus, LeaseGeneration,
    Phase, RunId, RunOrigin, SceneCut, SceneCuts, SceneHeader, SceneId, ScreenClass, ScreenHolds,
    ScreenOff, ThreadId,
};
use kumochi_scene::{
    Record, StopCut, deferred::Deferred, parse_record, parse_screen_draw_record,
    record::ScreenDisplay, screen, text::Bounded,
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
use super::tests::{after, chord, listen, native, until};
use crate::input::{Edge, Event, Key, TalkHeld};
use crate::link::{LinkEvent, LinkState};
use crate::platform::input::{Button, input};
use crate::platform::{Deliver, PresenceEvent, with_raw_keys, with_raw_modifiers};
use crate::presence::kumo::{Motion, v2};
use crate::presence::pulling::{Grab, PullSpace};
use crate::strings::Text;
use kumochi_shell_core::presenter::fixture::{scene_ids, select};

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
        self.tell(Told::Start(held.0[0].is_some(), held.0[1].is_some()));
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
    let held = TalkHeld([crate::input::PhysicalKey::new(0x1D), None]);
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
fn talk_escape_refusal_never_admits_a_hold_or_arms_a_screen_watch(cx: &mut TestAppContext) {
    let watching = watch(true, cx);
    watching
        .ready
        .owner
        .read_with(cx, |owner, _| owner.native.refuse_talk_escape_for_test());
    recognize(&watching.ready.owner, 1, cx);
    release(&watching.ready.owner, 1, Some(DOWN), cx);
    let told = told(&watching);
    assert!(told.iter().all(|t| !matches!(
        t,
        Told::Start(..)
            | Told::Sent(
                ControlToBrain::ScreenHoldRecognized { .. } | ControlToBrain::ScreenRelease { .. }
            )
    )));
    assert_eq!(
        told.iter()
            .filter(|t| **t
                == Told::Sent(ControlToBrain::VoiceHoldStopped {
                    gesture: GestureId(1)
                }))
            .count(),
        1
    );
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert!(!owner.talk_escape.held());
        assert!(!owner.link.input_is(GestureId(1)));
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
    let deliver = with_raw_modifiers(native(&watching.ready.owner, cx));
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
fn a_reset_before_recognition_delivery_retires_only_that_unstarted_generation(
    cx: &mut TestAppContext,
) {
    let watching = watch(true, cx);
    let deliver = native(&watching.ready.owner, cx);
    let at = Instant::now();
    cx.update(|_| {
        chord(&deliver, Edge::Down, at);
        deliver(Event::Deadline, after(at, 301));
        deliver(Event::Reset, after(at, 302));
    });
    cx.run_until_parked();
    assert!(told(&watching).iter().all(|message| !matches!(
        message,
        Told::Start(..)
            | Told::Sent(
                ControlToBrain::ScreenHoldRecognized { .. } | ControlToBrain::ScreenRelease { .. }
            )
    )));
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.link.microphone_gate().latest(), 1);
        assert!(owner.voice.pending.is_none());
    });

    chord(&deliver, Edge::Down, at + Duration::from_secs(1));
    deliver(Event::Deadline, after(at, 1_301));
    cx.run_until_parked();
    assert_eq!(
        told(&watching)
            .into_iter()
            .filter_map(|message| match message {
                Told::Sent(ControlToBrain::ScreenHoldRecognized { gesture, .. }) => Some(gesture),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [GestureId(2)],
        "the first generation must not occupy the next hold's gate"
    );
    deliver(Event::Reset, after(at, 1_302));
    cx.run_until_parked();
}

#[gpui::test]
fn overflow_keeps_the_third_key_down_until_its_own_break(cx: &mut TestAppContext) {
    needs_neutral(NeutralLoss::Overflow, cx);
}

#[gpui::test]
fn registered_escape_keeps_a_known_key_down_until_its_break(cx: &mut TestAppContext) {
    needs_neutral(NeutralLoss::Escape, cx);
    needs_neutral(NeutralLoss::NeutralEscape, cx);
}

#[gpui::test]
fn a_repeat_after_reset_is_not_neutral_and_the_first_fresh_chord_works(cx: &mut TestAppContext) {
    needs_neutral(NeutralLoss::Reset, cx);
}

#[derive(Clone, Copy, Debug)]
enum NeutralLoss {
    Overflow,
    Escape,
    NeutralEscape,
    Reset,
}

fn needs_neutral(loss: NeutralLoss, cx: &mut TestAppContext) {
    let watching = watch(true, cx);
    let deliver = native(&watching.ready.owner, cx);
    let raw = with_raw_keys(&deliver);
    let at = Instant::now();
    let key = |device, make, edge, ms| raw(device, make, edge, after(at, ms));
    let chord = |edge, ms| {
        key(1, 0x1D, edge, ms);
        key(1, 0x38, edge, ms + 1);
    };
    let unrelated = match loss {
        NeutralLoss::Overflow => {
            key(1, 0x25, Edge::Down, 0);
            key(2, 0x24, Edge::Down, 1);
            key(3, 0x2A, Edge::Down, 2);
            key(1, 0x25, Edge::Up, 3);
            key(2, 0x24, Edge::Up, 4);
            Some((3, 0x2A))
        }
        NeutralLoss::Escape => {
            key(1, 0x25, Edge::Down, 0);
            deliver(Event::Escape, after(at, 1));
            Some((1, 0x25))
        }
        NeutralLoss::NeutralEscape => {
            deliver(Event::Escape, after(at, 1));
            None
        }
        NeutralLoss::Reset => {
            key(1, 0x1D, Edge::Down, 0);
            key(1, 0xFF, Edge::Down, 1);
            key(1, 0x1D, Edge::Down, 2);
            key(1, 0x1D, Edge::Down, 3);
            key(1, 0x1D, Edge::Up, 4);
            None
        }
    };
    if let Some((device, make)) = unrelated {
        chord(Edge::Down, 20);
        chord(Edge::Up, 30);
        chord(Edge::Down, 700);
        deliver(Event::Deadline, after(at, 1_002));
        chord(Edge::Up, 1_010);
        cx.run_until_parked();
        assert!(
            told(&watching).iter().all(|message| !matches!(
                message,
                Told::Start(..)
                    | Told::Sent(
                        ControlToBrain::KeyDown { .. }
                            | ControlToBrain::VoiceHoldRecognized { .. }
                            | ControlToBrain::ScreenHoldRecognized { .. }
                            | ControlToBrain::ScreenRelease { .. }
                    )
            )),
            "{loss:?}: never warm, recognize, arm or send before the last break"
        );
        key(device, make, Edge::Up, 1_020);
    }
    cx.run_until_parked();
    chord(Edge::Down, 2_000);
    deliver(Event::Deadline, after(at, 2_302));
    cx.run_until_parked();
    assert_eq!(
        told(&watching)
            .into_iter()
            .filter_map(|message| match message {
                Told::Sent(ControlToBrain::ScreenHoldRecognized { gesture, .. }) => Some(gesture),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [GestureId(1)],
        "{loss:?}: the first fresh chord after observed breaks is eligible"
    );
    key(1, 0xFF, Edge::Down, 2_303);
    cx.run_until_parked();
}

#[gpui::test]
fn stale_releases_cannot_arm_or_send_at_either_delivery_boundary(cx: &mut TestAppContext) {
    for second in [false, true] {
        for reset in [false, true] {
            let watching = watch(true, cx);
            let deliver = Rc::new(with_raw_modifiers(native(&watching.ready.owner, cx)));
            let at = Instant::now();
            let finish = listen(&watching.ready.owner, &deliver, 1, at, cx);
            told(&watching);
            let advance = {
                let deliver = Rc::clone(&deliver);
                move || {
                    deliver(
                        Event::Key {
                            key: Key::LeftControl,
                            edge: Edge::Up,
                        },
                        after(at, 901),
                    );
                    deliver(
                        Event::Key {
                            key: Key::LeftControl,
                            edge: Edge::Down,
                        },
                        after(at, 902),
                    );
                    if reset {
                        deliver(Event::Reset, after(at, 903));
                    }
                }
            };
            let release = || {
                deliver(
                    Event::Key {
                        key: Key::LeftAlt,
                        edge: Edge::Up,
                    },
                    after(at, 900),
                )
            };
            if second {
                crate::presence::gesture::before_deferred_release(advance);
                release();
            } else {
                cx.update(|_| {
                    release();
                    advance();
                });
            }
            cx.run_until_parked();
            finish.send(()).unwrap();
            let terminals = Cell::new((0, 0));
            until(cx, |cx| {
                let messages = told(&watching);
                assert!(messages.iter().all(|t| !matches!(
                    t,
                    Told::Start(..) | Told::Sent(ControlToBrain::ScreenRelease { .. })
                )));
                let (mut stops, mut releases) = terminals.get();
                for message in messages {
                    match message {
                        Told::Sent(ControlToBrain::VoiceHoldStopped {
                            gesture: GestureId(1),
                        }) => stops += 1,
                        Told::Sent(ControlToBrain::KeyUp {
                            gesture: GestureId(1),
                            ..
                        }) => releases += 1,
                        _ => {}
                    }
                }
                terminals.set((stops, releases));
                stops > 0
                    && watching.ready.owner.read_with(cx, |owner, _| {
                        if reset {
                            owner.voice.pending.is_none()
                        } else {
                            releases > 0
                        }
                    })
            });
            assert_eq!(terminals.get(), (1, usize::from(!reset)));
            watching.ready.owner.read_with(cx, |owner, _| {
                assert!(owner.voice.watched.is_none());
                assert!(owner.exclusion.lease().is_none());
                assert!(!owner.talk_escape.held());
            });
        }
    }
}

#[gpui::test]
fn an_older_failed_send_cannot_revoke_the_successors_watch_or_exclusion(cx: &mut TestAppContext) {
    let watching = released(true, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        owner
            .voice
            .pending
            .as_ref()
            .unwrap()
            .token
            .discard_unstarted()
            .unwrap();
        owner.capture_finished(1, CaptureFor::Gesture(GestureId(1)), cx);
    });
    state(&watching, RUN, Phase::Answering, HOLD, cx);
    hold(&watching, 2, cx);
    release(&watching.ready.owner, 2, Some(DOWN), cx);
    let second = told(&watching);
    assert!(second.iter().any(|t| matches!(t, Told::Start(..))));
    assert!(second.contains(&Told::Sent(screen_release(2, 2, DOWN, &[]))));
    watching.ready.owner.update(cx, |owner, cx| {
        owner.screen_release_failed(GestureId(1), LeaseGeneration(1), cx);
        assert!(owner.voice.watched.is_some());
        assert!(owner.exclusion.live(LeaseGeneration(2)));
        assert_eq!(owner.screen_lease, Some(LeaseGeneration(2)));
    });
    assert_eq!(sources(&watching), []);
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

    // The hold's own `RunEnd` that comes before its first state is kept for it.
    let watching = released(true, cx);
    end(&watching, RUN, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    assert_eq!(sources(&watching), []);
    state(&watching, RUN, Phase::Done, None, cx);
    assert_eq!(sources(&watching), [Told::Stop]);

    // Another run that ends before the hold's is named ends nothing: the sources listen on,
    // and the hold's marks then show.
    let watching = released(true, cx);
    let hidden = detached(&watching.ready, cx);
    watching.ready.owner.update(cx, |owner, _| {
        owner.overlay = Visibility::Hidden(hidden);
    });
    state(&watching, 3, Phase::Answering, None, cx);
    state(&watching, 3, Phase::Done, None, cx);
    end(&watching, 3, cx);
    assert_eq!(told(&watching), []);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    mark(&watching, MARKS, RUN, DISPLAY, cx);
    cx.run_until_parked();
    let (run, scene, index) = (RunId(RUN), MARKS, 0);
    assert_eq!(
        told(&watching),
        [
            Told::Sent(ControlToBrain::StepReady { run, scene, index }),
            Told::Sent(ControlToBrain::StepShown { run, scene, index }),
        ]
    );
}

/// A send that wrote nothing tells no `HoldScreen`: the run's failure still ends the watch, once
/// its terminal state and its `RunEnd` are both in, in either order.
#[gpui::test]
fn a_hold_told_nothing_ends_the_watch_once_its_failed_run_has_both_endings(
    cx: &mut TestAppContext,
) {
    for state_first in [true, false] {
        let watching = released(true, cx);
        state(&watching, RUN, Phase::Thinking, HOLD, cx);
        assert_eq!(sources(&watching), []);
        if state_first {
            state(&watching, RUN, Phase::Failed, None, cx);
        } else {
            end(&watching, RUN, cx);
        }
        assert_eq!(sources(&watching), [], "one ending is in");
        if state_first {
            end(&watching, RUN, cx);
        } else {
            state(&watching, RUN, Phase::Failed, None, cx);
        }
        assert_eq!(
            sources(&watching),
            [Told::Stop],
            "state first: {state_first}"
        );
    }
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
        owner.voice.gates.received(&hold.token);
    });
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    assert_eq!(told(&watching), []);
    cx.run_until_parked();
    // The scripted source never ran, so its microphone stop is told here and acknowledged.
    watching.ready.owner.read_with(cx, |owner, _| {
        let session = owner.session.as_ref().expect("connected");
        let stopped = ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(1),
        };
        assert!(owner.link.send_control(session, stopped.clone()));
        assert_eq!(told(&watching), [Told::Sent(stopped)]);
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
    });
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

    // Another run's end leaves the release's limit where it was.
    let watching = released(true, cx);
    let limit = watchdog(&watching, cx);
    assert!(limit.is_some());
    state(&watching, 3, Phase::Answering, None, cx);
    state(&watching, 3, Phase::Done, None, cx);
    end(&watching, 3, cx);
    assert_eq!(watchdog(&watching, cx), limit);
    assert_eq!(sources(&watching), []);

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
    // An ordinary drawing's overlay, when one is open, takes the marks as well.
    let open = |owner: &Presence, _: &gpui_kit::App| owner.overlay.window().is_some();
    if !watching.ready.owner.read_with(cx, open) {
        let hidden = detached(&watching.ready, cx);
        watching.ready.owner.update(cx, |owner, _| {
            owner.overlay = Visibility::Hidden(hidden);
        });
    }
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

// The tests below that press Esc on a pending hold pin its cancel as it is told today: twice
// for one press, once where the hold's input is revoked and once from the runs' hold.

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

/// Hold 1 is made and let go through the native callback, so the callback's reducer still
/// names it, and it is pending with its speech sent.
fn natively_pending(watching: &Watching, cx: &mut TestAppContext) -> (Deliver, Instant) {
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
    told(watching);
    (deliver, at)
}

/// Hold 1 is pending as [`natively_pending`] leaves it, and its first mark overtook its run's
/// `Answering`: the marks are up while the hold is pending.
fn natively_marked(cx: &mut TestAppContext) -> (Watching, Deliver, Instant) {
    let watching = watch(true, cx);
    let (deliver, at) = natively_pending(&watching, cx);
    mark_up(&watching, false, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice_working(), "the hold is still pending");
        assert!(owner.text_open());
    });
    (watching, deliver, at)
}

/// The press took the marks and cancelled the hold, and the panel stays open.
fn only_taken(watching: &Watching, cx: &mut TestAppContext) {
    ended(watching, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.marks_shown(), None);
        assert!(owner.text_open());
    });
}

#[gpui::test]
fn esc_through_the_hotkey_s_reducer_takes_a_pending_hold_s_marks_and_cancels_it(
    cx: &mut TestAppContext,
) {
    // Kumochi is in front, so the sources don't count the key: the reducer's own cancel ends
    // the hold first, and the press then takes the marks.
    let (watching, deliver, at) = natively_marked(cx);
    let (key, edge) = (
        Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
        Edge::Down,
    );
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
                owner
                    .on_event(PresenceEvent::ScreenWoke, cx)
                    .expect("a wake");
            });
        };
        let (key, edge) = (
            Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
            Edge::Down,
        );
        deliver(Event::Key { key, edge }, after(at, 2000));
        let heard = watching
            .bit
            .borrow()
            .as_ref()
            .expect("started sources")
            .set();
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

/// An earlier answer's ordinary drawing, on the overlay.
const DRAWN: SceneId = SceneId(40);

/// `run` draws `scene` on the overlay: its begin and its first step, which is shown.
fn draw(watching: &Watching, run: u64, scene: SceneId, cx: &mut TestAppContext) {
    let begin = format!(
        r#"{{"begin":{{"v":1,"scene":"s{}","surface":"overlay","alt":""}}}}"#,
        scene.0
    );
    let step =
        br#"{"step":{"id":"s","put":[{"shape":{"id":"b","kind":"rect","box":[0,0,10,10]}}]}}"#;
    let (Ok(Record::Begin(begin)), Ok(Record::Step(step))) =
        (parse_record(begin.as_bytes()), parse_record(step))
    else {
        panic!("an overlay scene's records");
    };
    let (run, index) = (RunId(run), 0);
    let header = SceneHeader {
        begin,
        layout_version: 2,
    };
    for frame in [
        DataToShell::SceneBegin { scene, header },
        DataToShell::SceneStepCommit {
            run,
            scene,
            index,
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
    cx.run_until_parked();
}

/// Run 3 drew `DRAWN` on the overlay and ended, and one Esc stopped the drawing's motion: an
/// ordinary drawing is up and nothing moves.
fn drawn(watching: &Watching, cx: &mut TestAppContext) {
    let hidden = detached(&watching.ready, cx);
    watching.ready.owner.update(cx, |owner, _| {
        owner.overlay = Visibility::Hidden(hidden);
        // A release now covers the drawing's window too, which the fixture's stamp reader
        // would refuse: it knows only its own four surfaces.
        owner.counter_fixture = Some(Rc::new(|| 4_242));
    });
    state(watching, 3, Phase::Answering, None, cx);
    draw(watching, 3, DRAWN, cx);
    state(watching, 3, Phase::Done, None, cx);
    end(watching, 3, cx);
    watching
        .ready
        .owner
        .update(cx, Presence::escape)
        .expect("escape");
    cx.run_until_parked();
    told(watching);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(scene_ids(&owner.presenter), [DRAWN]);
        assert!(owner.activity.drawing);
        assert!(!owner.activity.working);
    });
}

/// The answer in the panel and whether the panel is open.
fn panel(watching: &Watching, cx: &mut TestAppContext) -> (Option<RunId>, bool) {
    watching.ready.owner.read_with(cx, |owner, _| {
        (owner.runs.shown().map(|shown| shown.run), owner.text_open())
    })
}

/// The press cancelled hold 1 and did nothing else, as [`nothing_else`] says.
fn only_cancelled(watching: &Watching, before: (Option<RunId>, bool), cx: &mut TestAppContext) {
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldCancelled));
    });
    nothing_else(watching, before, cx);
}

/// Hold 1 is gone and its press did nothing else: the ordinary drawing is still up and the
/// panel is as it was.
fn nothing_else(watching: &Watching, before: (Option<RunId>, bool), cx: &mut TestAppContext) {
    ended(watching, cx);
    assert_eq!(panel(watching, cx), before);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.marks_shown(), None);
        assert_eq!(scene_ids(&owner.presenter), [DRAWN]);
        assert!(owner.runs.drawing());
        assert!(owner.activity.drawing);
        assert!(!owner.activity.working, "the press left a current activity");
    });
}

#[gpui::test]
fn a_press_that_cancelled_its_pending_hold_leaves_an_ordinary_drawing_up(cx: &mut TestAppContext) {
    for marks in [true, false] {
        // Kumochi is in front (`None`), or another app is and the sources' wake is handled
        // before the hotkey's press or after it.
        for woke_first in [None, Some(true), Some(false)] {
            let watching = watch(true, cx);
            drawn(&watching, cx);
            let (deliver, at) = natively_pending(&watching, cx);
            if marks {
                mark_up(&watching, false, cx);
            }
            let before = panel(&watching, cx);
            assert!(!marks || before.1, "with marks up the panel is open");
            let wake = |cx: &mut TestAppContext| {
                watching.ready.owner.update(cx, |owner, cx| {
                    owner
                        .on_event(PresenceEvent::ScreenWoke, cx)
                        .expect("a wake");
                });
            };
            let (key, edge) = (
                Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
                Edge::Down,
            );
            deliver(Event::Key { key, edge }, after(at, 2000));
            if woke_first.is_some() {
                let bit = watching.bit.borrow();
                bit.as_ref().expect("started sources").set();
            }
            if woke_first == Some(true) {
                wake(cx);
            }
            deliver(Event::Escape, after(at, 2000));
            cx.run_until_parked();
            if woke_first == Some(false) {
                wake(cx);
            }
            // The hold's cancels, the sources' stop and the marks' clear, and nothing else:
            // no `Stop`, and no `ClearDrawing` for the ordinary drawing.
            let [stop, clear] = gone();
            let expected = match (marks, woke_first) {
                (true, Some(true)) => vec![cancel(), stop, clear, cancel()],
                (true, _) => vec![cancel(), cancel(), stop, clear],
                (false, _) => vec![cancel(), stop, cancel()],
            };
            assert_eq!(
                told(&watching),
                expected,
                "marks {marks}, woke first {woke_first:?}"
            );
            only_cancelled(&watching, before, cx);
        }
    }
}

/// Hold 1 is recognized through the native callback and still waits for its microphone, with
/// the talk keys down; then the raw Esc-down ends it in a delivery of its own.
fn ended_by_esc_down(watching: &Watching, cx: &mut TestAppContext) -> (Deliver, Instant) {
    let owner = watching.ready.owner.clone();
    let deliver = native(&owner, cx);
    let at = Instant::now();
    for key in [Key::LeftControl, Key::LeftAlt] {
        let edge = Edge::Down;
        deliver(Event::Key { key, edge }, after(at, 0));
    }
    deliver(Event::Deadline, after(at, 301));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        let hold = owner.voice.pending.as_ref().expect("the hold");
        assert!(matches!(hold.source, Source::Waiting));
    });
    let (key, edge) = (
        Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
        Edge::Down,
    );
    deliver(Event::Key { key, edge }, after(at, 600));
    cx.run_until_parked();
    owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.voice.ended, Some((GestureId(1), None)));
    });
    (deliver, at)
}

#[gpui::test]
fn the_hotkey_s_press_after_an_esc_down_that_ended_a_waiting_hold_does_nothing_more(
    cx: &mut TestAppContext,
) {
    let watching = watch(true, cx);
    drawn(&watching, cx);
    let before = panel(&watching, cx);
    let (deliver, at) = ended_by_esc_down(&watching, cx);
    told(&watching);
    deliver(Event::Escape, after(at, 601));
    cx.run_until_parked();
    assert_eq!(told(&watching), []);
    only_cancelled(&watching, before, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.ended, None, "the press took the receipt");
    });
}

/// The app is borrowed while the press arrives, so its delivery waits in the callback's
/// backlog: hold 1's input is revoked and its cancel is written, and nothing is applied yet.
fn press_deferred(deliver: &Deliver, at: Instant, cx: &mut TestAppContext) {
    cx.update(|_| {
        let (key, edge) = (
            Key::Escape(crate::input::PhysicalKey::new(1).unwrap()),
            Edge::Down,
        );
        deliver(Event::Key { key, edge }, after(at, 2000));
        deliver(Event::Escape, after(at, 2000));
    });
}

#[gpui::test]
fn a_reply_that_ends_the_hold_before_its_deferred_press_leaves_an_ordinary_drawing_up(
    cx: &mut TestAppContext,
) {
    // The brain's answer to the press's cancel is applied before the press is: the hold's
    // `Cancelled` status, or its run's `Interrupted` and end once `Thinking` had named it.
    for interrupted in [false, true] {
        let watching = watch(true, cx);
        drawn(&watching, cx);
        let (deliver, at) = natively_pending(&watching, cx);
        if interrupted {
            state(&watching, RUN, Phase::Thinking, HOLD, cx);
        }
        cx.run_until_parked();
        told(&watching);
        let before = panel(&watching, cx);
        press_deferred(&deliver, at, cx);
        watching.ready.owner.read_with(cx, |owner, _| {
            assert!(owner.voice_working(), "the press is not applied yet");
        });
        if interrupted {
            state(&watching, RUN, Phase::Interrupted, None, cx);
            end(&watching, RUN, cx);
        } else {
            status(&watching.ready.owner, 1, 1, HoldStatus::Cancelled, cx);
        }
        watching.ready.owner.read_with(cx, |owner, _| {
            assert!(owner.voice.pending.is_none(), "the reply ended the hold");
        });
        cx.run_until_parked();
        // The press's cancel and what the reply itself did, and nothing from the press after
        // it: no `Stop`, and no `ClearDrawing` for the ordinary drawing.
        let expected = if interrupted {
            vec![cancel(), Told::Stop]
        } else {
            vec![cancel(), Told::Stop, cancel()]
        };
        assert_eq!(told(&watching), expected, "interrupted {interrupted}");
        let notice = (!interrupted).then_some(Text::HoldCancelled);
        watching.ready.owner.read_with(cx, |owner, _| {
            assert_eq!(owner.voice.status, notice);
        });
        nothing_else(&watching, before, cx);
    }
}

/// An ordinary drawing is up, hold 1 is pending with its speech sent, and `Thinking` has
/// named its run.
fn thinking(cx: &mut TestAppContext) -> (Watching, Deliver, Instant) {
    let watching = watch(true, cx);
    drawn(&watching, cx);
    let (deliver, at) = natively_pending(&watching, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    cx.run_until_parked();
    told(&watching);
    (watching, deliver, at)
}

#[gpui::test]
#[ignore = "isolated quiet-Rez captured-stop and intake timing; no native window or input"]
#[expect(
    clippy::too_many_lines,
    reason = "keep the bounded before/after input sequence together"
)]
fn measure_captured_stop_and_raw_intake(cx: &mut TestAppContext) {
    use crate::allocation;
    use crate::link::Batch;
    use kumochi_protocol::frame::{self, Pipe};
    use kumochi_shell_core::presenter::fixture::{header, rect};

    let _timeout = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(90));
        std::process::exit(124);
    });
    println!(
        "captured_fixed_storage hold_bytes={} voice_bytes={} runs_bytes={}",
        std::mem::size_of::<super::Hold>(),
        std::mem::size_of::<super::Voice>(),
        std::mem::size_of::<crate::runs::Runs>()
    );
    for (mode, stopped, automatic) in [
        ("live", false, false),
        ("named", true, false),
        ("automatic", true, true),
    ] {
        for iteration in 0..37 {
            let watching = watch(true, cx);
            drawn(&watching, cx);
            let mut registrations = Vec::new();
            let mut automatic_names = Vec::new();
            for gesture in [1, 2, 3] {
                hold(&watching, gesture, cx);
                release(&watching.ready.owner, gesture, Some(DOWN), cx);
                watching.ready.owner.update(cx, |owner, _| {
                    let hold = owner.voice.pending.as_mut().expect("released hold");
                    hold.token
                        .discard_unstarted()
                        .expect("the fixture never starts a source");
                    hold.source = Source::Sent;
                });
                let run = 6 + u64::from(gesture);
                let cancel = |cx: &mut TestAppContext| {
                    watching.ready.owner.update(cx, |owner, cx| {
                        let start = Instant::now();
                        let (result, counts, bytes) =
                            allocation::usage(|| owner.native_escape(Some(GestureId(gesture)), cx));
                        (result, start.elapsed().as_nanos(), counts, bytes)
                    })
                };
                if automatic && gesture < 3 {
                    let row = cancel(cx);
                    row.0.expect("captured pre-name cancel");
                    registrations.push((gesture, row.1, row.2, row.3));
                    let batch = Batch::Plain(vec![LinkEvent::RunState {
                        generation: 1,
                        run: RunId(run),
                        thread: ThreadId(1),
                        phase: Phase::Thinking,
                        error: None,
                        origin: Some(RunOrigin::Hold {
                            gesture: GestureId(gesture),
                        }),
                    }]);
                    let row = watching.ready.owner.update(cx, |owner, cx| {
                        let start = Instant::now();
                        let (result, counts, bytes) =
                            allocation::usage(|| owner.link_events(batch, cx));
                        (result, start.elapsed().as_nanos(), counts, bytes)
                    });
                    row.0.expect("automatic late-name stop");
                    automatic_names.push((gesture, row.1, row.2, row.3));
                } else {
                    state(
                        &watching,
                        run,
                        Phase::Thinking,
                        Some(RunOrigin::Hold {
                            gesture: GestureId(gesture),
                        }),
                        cx,
                    );
                }
                state(&watching, run, Phase::Answering, None, cx);
                told(&watching);
                if stopped && !automatic && gesture < 3 {
                    let row = cancel(cx);
                    row.0.expect("captured stop");
                    registrations.push((gesture, row.1, row.2, row.3));
                }
                cx.run_until_parked();
                told(&watching);
            }
            let mut events = Vec::new();
            for (run, scene) in [(7, SceneId(50)), (8, SceneId(51)), (9, SceneId(52))] {
                for message in [
                    DataToShell::SceneBegin {
                        scene,
                        header: header(),
                    },
                    DataToShell::SceneStepCommit {
                        run: RunId(run),
                        scene,
                        index: 0,
                        step: rect("measured"),
                    },
                ] {
                    let bytes = u32::try_from(
                        frame::encode(&message, Pipe::Data)
                            .expect("bounded frame")
                            .len(),
                    )
                    .expect("frame size");
                    events.push(LinkEvent::SceneData {
                        generation: 1,
                        frame: Deferred::new(message, 0),
                        bytes,
                    });
                }
            }
            for run in [7, 8, 9] {
                let message = DataToShell::RunEnd { run: RunId(run) };
                let bytes = u32::try_from(
                    frame::encode(&message, Pipe::Data)
                        .expect("bounded end")
                        .len(),
                )
                .expect("frame size");
                events.push(LinkEvent::Data {
                    generation: 1,
                    message,
                    bytes,
                });
            }
            for run in [7, 8] {
                events.push(LinkEvent::RunState {
                    generation: 1,
                    run: RunId(run),
                    thread: ThreadId(1),
                    phase: Phase::Done,
                    error: None,
                    origin: None,
                });
            }
            assert_eq!(events.len(), 11);
            let row = watching.ready.owner.update(cx, |owner, cx| {
                let start = Instant::now();
                let (result, counts, bytes) =
                    allocation::usage(|| owner.link_events(Batch::Plain(events), cx));
                (result, start.elapsed().as_nanos(), counts, bytes)
            });
            row.0.expect("raw intake");
            watching.ready.owner.read_with(cx, |owner, _| {
                assert_eq!(owner.runs.stoppable(), Some(RunId(9)));
                assert!(owner.session.is_some());
            });
            cx.run_until_parked();
            told(&watching);
            if iteration >= 5 {
                println!(
                    "captured_intake mode={mode} stopped={stopped} sample={} events=11 ns={} allocations={} releases={} allocated_bytes={} released_bytes={}; UI dispatch/application, raw ends precede local admission drain, unconsumed third receipt observes its raw end; no IPC/decode, raw input, worker completion or GPU paint",
                    iteration - 5,
                    row.1,
                    row.2[0],
                    row.2[1],
                    row.3[0],
                    row.3[1]
                );
                for (gesture, ns, counts, bytes) in registrations {
                    println!(
                        "captured_registration mode={mode} gesture={gesture} sample={} ns={ns} allocations={} releases={} allocated_bytes={} released_bytes={}; complete captured UI operation, not map-only cost",
                        iteration - 5,
                        counts[0],
                        counts[1],
                        bytes[0],
                        bytes[1]
                    );
                }
                for (gesture, ns, counts, bytes) in automatic_names {
                    println!(
                        "captured_first_name mode={mode} gesture={gesture} sample={} ns={ns} allocations={} releases={} allocated_bytes={} released_bytes={}; complete first-state application after captured cancellation, not map-only cost",
                        iteration - 5,
                        counts[0],
                        counts[1],
                        bytes[0],
                        bytes[1]
                    );
                }
            }
        }
    }
}

#[gpui::test]
fn captured_stop_preserves_an_older_moving_scene_and_a_pending_typed_turn(cx: &mut TestAppContext) {
    let watching = watch(true, cx);
    drawn(&watching, cx);
    watching.ready.owner.update(cx, |owner, _| {
        owner
            .runs
            .submit("keep this queued turn", |_| true)
            .expect("queued before voice recognition");
    });
    let (deliver, at) = natively_pending(&watching, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    let older = SceneId(41);
    draw(&watching, 6, older, cx);
    draw(&watching, RUN, OTHER, cx);
    told(&watching);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    cx.run_until_parked();
    assert_eq!(
        told(&watching),
        [cancel(), Told::Stop, stopped(vec![cut(OTHER)])]
    );
    watching.ready.owner.update(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), None);
        assert_eq!(owner.presenter.stoppable_run(), Some(RunId(6)));
        assert_eq!(scene_ids(&owner.presenter), [OTHER, DRAWN, older]);
        let applied = owner.runs.apply_state(
            RunId(8),
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        );
        assert_eq!(applied.stop, None);
        assert!(applied.visible);
        assert_eq!(owner.runs.stoppable(), Some(RunId(8)));
    });
}

#[gpui::test]
fn captured_stop_does_not_promote_its_receipt_to_a_newer_presenter_run(cx: &mut TestAppContext) {
    let (watching, deliver, at) = thinking(cx);
    draw(&watching, RUN + 1, OTHER, cx);
    told(&watching);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    cx.run_until_parked();
    assert_eq!(told(&watching), [cancel(), Told::Stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), Some(RunId(RUN)));
        assert_eq!(owner.presenter.stoppable_run(), Some(RunId(RUN + 1)));
        assert_eq!(scene_ids(&owner.presenter), [OTHER, DRAWN]);
    });
}

#[gpui::test]
fn captured_cancel_before_first_state_never_promotes_its_automatic_stop(cx: &mut TestAppContext) {
    let watching = watch(true, cx);
    drawn(&watching, cx);
    watching.ready.owner.update(cx, |owner, _| {
        owner
            .runs
            .submit("keep this queued turn", |_| true)
            .expect("queued before voice recognition");
    });
    let (deliver, at) = natively_pending(&watching, cx);
    let older = SceneId(41);
    draw(&watching, 6, older, cx);
    draw(&watching, RUN + 1, OTHER, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.presenter.live_work(Instant::now()));
        assert_eq!(owner.presenter.stoppable_run(), Some(RunId(RUN + 1)));
        assert_eq!(
            owner.voice.pending.as_ref().expect("pending hold").run,
            None
        );
    });
    told(&watching);
    press_deferred(&deliver, at, cx);
    cx.run_until_parked();
    assert_eq!(told(&watching), [cancel(), Told::Stop, cancel()]);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    cx.run_until_parked();
    assert_eq!(
        told(&watching),
        [Told::Sent(ControlToBrain::Stop {
            run: RunId(RUN),
            revealed: Vec::new(),
        })]
    );
    watching.ready.owner.update(cx, |owner, _| {
        assert_eq!(owner.presenter.stoppable_run(), Some(RunId(RUN + 1)));
        assert_eq!(scene_ids(&owner.presenter), [OTHER, DRAWN, older]);
        let typed = owner.runs.apply_state(
            RunId(RUN + 2),
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        );
        assert!(typed.visible);
        assert_eq!(typed.stop, None);
        assert_eq!(owner.runs.stoppable(), Some(RunId(RUN + 2)));
    });
}

#[gpui::test]
fn automatic_hold_stop_keeps_newer_voice_ownership_and_folds_a_prebind_raw_end(
    cx: &mut TestAppContext,
) {
    let watching = watch(true, cx);
    drawn(&watching, cx);
    let (deliver, at) = natively_pending(&watching, cx);
    press_deferred(&deliver, at, cx);
    cx.run_until_parked();
    told(&watching);
    end(&watching, RUN, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
    });
    hold(&watching, 2, cx);
    release(&watching.ready.owner, 2, Some(DOWN), cx);
    watching.ready.owner.update(cx, |owner, _| {
        let hold = owner.voice.pending.as_mut().expect("newer pending hold");
        hold.token.discard_unstarted().expect("no source started");
        hold.source = Source::Sent;
        assert_eq!(hold.token.gesture(), GestureId(2));
        assert_eq!(hold.run, None);
        assert_eq!(owner.voice.ended, None);
    });
    told(&watching);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    cx.run_until_parked();
    assert_eq!(
        told(&watching),
        [Told::Sent(ControlToBrain::Stop {
            run: RunId(RUN),
            revealed: Vec::new(),
        })]
    );
    watching.ready.owner.read_with(cx, |owner, _| {
        let hold = owner.voice.pending.as_ref().expect("newer hold survives");
        assert_eq!(hold.token.gesture(), GestureId(2));
        assert_eq!(hold.run, None);
        assert_eq!(owner.voice.ended, None);
        assert_eq!(
            owner.runs.owned_disposition(RunId(RUN)),
            crate::runs::RunDisposition::Stopped
        );
    });
    state(&watching, RUN, Phase::Done, None, cx);
    state(
        &watching,
        RUN + 1,
        Phase::Thinking,
        Some(RunOrigin::Hold {
            gesture: GestureId(2),
        }),
        cx,
    );
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.runs.owned_disposition(RunId(RUN)),
            crate::runs::RunDisposition::Live
        );
        let hold = owner.voice.pending.as_ref().expect("newer named hold");
        assert_eq!(hold.token.gesture(), GestureId(2));
        assert_eq!(hold.run.expect("newer run").run, RunId(RUN + 1));
        assert_eq!(owner.voice.ended, None);
        assert_eq!(owner.runs.stoppable(), Some(RunId(RUN + 1)));
    });
}

#[gpui::test]
fn automatic_hold_stop_cuts_pre_named_work_and_tags_later_commits(cx: &mut TestAppContext) {
    use kumochi_protocol::message::SceneFailure;
    use kumochi_shell_core::presenter::fixture::{object_count, step, tip_revision};

    let watching = watch(true, cx);
    drawn(&watching, cx);
    let (deliver, at) = natively_pending(&watching, cx);
    let owned = SceneId(42);
    let older = SceneId(41);
    draw(&watching, 6, older, cx);
    draw(&watching, RUN, owned, cx);
    draw(&watching, RUN + 1, OTHER, cx);
    told(&watching);
    press_deferred(&deliver, at, cx);
    cx.run_until_parked();
    told(&watching);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    cx.run_until_parked();
    assert_eq!(told(&watching), [stopped(vec![cut(owned)])]);
    for frame in [
        DataToShell::SceneCutApplied {
            scene: owned,
            revision: 1,
            cut: cut(owned).cut,
        },
        DataToShell::SceneStepCommit {
            run: RunId(RUN),
            scene: owned,
            index: 1,
            step: step(r#"{"step":{"id":"late"}}"#),
        },
    ] {
        receive(
            &watching.ready.owner,
            LinkEvent::SceneData {
                generation: 1,
                frame: Deferred::new(frame, 0),
                bytes: 0,
            },
            cx,
        );
    }
    cx.run_until_parked();
    assert_eq!(
        told(&watching),
        [Told::Sent(ControlToBrain::StepFailed {
            run: RunId(RUN),
            scene: owned,
            index: 1,
            reason: SceneFailure::Cancelled,
            cut: StopCut {
                activated: 1,
                revealed: Bounded::default()
            },
        })]
    );
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.stoppable_run(), Some(RunId(RUN + 1)));
        assert_eq!(scene_ids(&owner.presenter), [OTHER, DRAWN, older, owned]);
        assert_eq!(tip_revision(&owner.presenter, owned), 1);
        assert_eq!(object_count(&owner.presenter, OTHER), 1);
        assert_eq!(object_count(&owner.presenter, older), 1);
    });
    receive(
        &watching.ready.owner,
        LinkEvent::SceneData {
            generation: 1,
            frame: Deferred::new(
                DataToShell::SceneStepCommit {
                    run: RunId(RUN),
                    scene: owned,
                    index: 2,
                    step: step(r#"{"step":{"id":"later"}}"#),
                },
                0,
            ),
            bytes: 0,
        },
        cx,
    );
    cx.run_until_parked();
    assert_eq!(told(&watching), []);
}

#[gpui::test]
fn voice_receipt_keeps_a_prebind_raw_end_and_terminal_closure_through_teardown(
    cx: &mut TestAppContext,
) {
    let watching = watch(true, cx);
    let (_deliver, _at) = natively_pending(&watching, cx);
    end(&watching, RUN, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        let tail = owner
            .voice
            .pending
            .as_ref()
            .expect("pending voice")
            .run
            .expect("bound receipt");
        assert_eq!(
            tail,
            crate::runs::RunTail {
                run: RunId(RUN),
                thread: ThreadId(1),
                control_closed: false,
                data_ended: true
            }
        );
    });
    state(&watching, RUN, Phase::Answering, None, cx);
    state(&watching, RUN, Phase::Done, None, cx);
    state(&watching, RUN, Phase::Thinking, None, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(
            owner.voice.ended,
            Some((
                GestureId(1),
                Some(crate::runs::RunTail {
                    run: RunId(RUN),
                    thread: ThreadId(1),
                    control_closed: true,
                    data_ended: true
                })
            ))
        );
    });
}

#[gpui::test]
fn voice_receipt_accepts_only_owned_successful_delete_closure(cx: &mut TestAppContext) {
    use kumochi_protocol::message::Deleted;
    for outcome in [Deleted::Hidden, Deleted::Removed] {
        for raw_first in [false, true] {
            for last_run in [None, Some(RunId(RUN))] {
                let (watching, _deliver, _at) = thinking(cx);
                status(&watching.ready.owner, 1, 1, HoldStatus::Cancelled, cx);
                watching.ready.owner.update(cx, |owner, cx| {
                    owner.thread_deleted(ThreadId(1), outcome, last_run, cx);
                    assert!(
                        !owner
                            .voice
                            .ended
                            .expect("receipt")
                            .1
                            .expect("run")
                            .control_closed
                    );
                    assert!(owner.runs.confirm_delete(ThreadId(1)));
                    owner.thread_deleted(ThreadId(1), Deleted::NotHidden, last_run, cx);
                    assert!(
                        !owner
                            .voice
                            .ended
                            .expect("receipt")
                            .1
                            .expect("run")
                            .control_closed
                    );
                    assert!(owner.runs.confirm_delete(ThreadId(2)));
                    owner.thread_deleted(ThreadId(2), outcome, None, cx);
                    assert!(
                        !owner
                            .voice
                            .ended
                            .expect("receipt")
                            .1
                            .expect("run")
                            .control_closed
                    );
                    assert!(owner.runs.confirm_delete(ThreadId(1)));
                });
                if raw_first {
                    end(&watching, RUN, cx);
                }
                watching.ready.owner.update(cx, |owner, cx| {
                    owner.thread_deleted(ThreadId(1), outcome, last_run, cx);
                });
                if !raw_first {
                    end(&watching, RUN, cx);
                }
                watching.ready.owner.read_with(cx, |owner, _| {
                    assert_eq!(
                        owner.voice.ended,
                        Some((
                            GestureId(1),
                            Some(crate::runs::RunTail {
                                run: RunId(RUN),
                                thread: ThreadId(1),
                                control_closed: true,
                                data_ended: true
                            })
                        ))
                    );
                });
            }
        }
    }
}

#[gpui::test]
fn a_suppressed_terminal_updates_its_receipt_before_retiring_the_pending_input(
    cx: &mut TestAppContext,
) {
    let (watching, _deliver, _at) = thinking(cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    end(&watching, RUN, cx);
    watching.ready.owner.update(cx, |owner, _| {
        let tail = owner.voice.ended.expect("ended hold").1.expect("bound run");
        assert!(!tail.control_closed);
        assert!(tail.data_ended);
        assert_eq!(
            owner.runs.stop_owned(tail),
            Some(ControlToBrain::Stop {
                run: RunId(RUN),
                revealed: Vec::new()
            })
        );
    });
    state(&watching, RUN, Phase::Done, None, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.runs.owned_disposition(RunId(RUN)),
            crate::runs::RunDisposition::Live
        );
        assert_eq!(
            owner.voice.ended,
            Some((
                GestureId(1),
                Some(crate::runs::RunTail {
                    run: RunId(RUN),
                    thread: ThreadId(1),
                    control_closed: true,
                    data_ended: true
                })
            ))
        );
    });
}

/// What stopping run 7 tells while the presenter holds scenes: its cuts.
fn stopped(cuts: Vec<SceneCut>) -> Told {
    Told::Sent(ControlToBrain::StopScenes {
        run: RunId(RUN),
        cuts: SceneCuts::new(cuts).expect("cuts"),
    })
}

/// The cut of a scene stopped while its first step enters.
fn cut(scene: SceneId) -> SceneCut {
    let cut = StopCut {
        activated: 1,
        revealed: Bounded::default(),
    };
    SceneCut { scene, cut }
}

#[gpui::test]
fn a_deferred_press_whose_answer_already_ended_does_nothing(cx: &mut TestAppContext) {
    let (watching, deliver, at) = thinking(cx);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    // The answer took the panel; the press, applied now, changes nothing in it.
    let before = panel(&watching, cx);
    cx.run_until_parked();
    assert_eq!(told(&watching), [cancel(), Told::Stop]);
    nothing_else(&watching, before, cx);
}

#[gpui::test]
fn a_deferred_press_stops_its_done_run_before_the_run_s_end(cx: &mut TestAppContext) {
    // The run's state is `Done` and its data's end is still to come.
    let (watching, deliver, at) = thinking(cx);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Done, None, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none());
        assert_eq!(owner.runs.stoppable(), Some(RunId(RUN)));
    });
    cx.run_until_parked();
    assert_eq!(told(&watching), [cancel(), Told::Stop, stopped(Vec::new())]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), None);
        assert_eq!(scene_ids(&owner.presenter), [DRAWN]);
    });
}

#[gpui::test]
fn a_deferred_press_stops_what_its_ended_run_still_draws(cx: &mut TestAppContext) {
    // The run drew without answering and has ended both ways; its drawing still moves.
    let (watching, deliver, at) = thinking(cx);
    draw(&watching, RUN, OTHER, cx);
    told(&watching);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        let now = Instant::now();
        assert_eq!(owner.runs.stoppable(), None);
        assert_eq!(owner.moving_run(now), Some(RunId(RUN)));
        // Once that drawing is at rest nothing moves, and a press would stop nothing.
        assert_eq!(owner.presenter.stoppable_run(), Some(RunId(RUN)));
        assert_eq!(owner.moving_run(now + Duration::from_secs(60)), None);
    });
    cx.run_until_parked();
    let stop = stopped(vec![cut(OTHER)]);
    assert_eq!(told(&watching), [cancel(), Told::Stop, stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.stoppable_run(), None);
        assert_eq!(scene_ids(&owner.presenter), [OTHER, DRAWN]);
        assert!(!owner.activity.working);
    });
}

#[gpui::test]
fn a_deferred_press_stops_what_a_cancelled_hold_s_run_still_draws(cx: &mut TestAppContext) {
    // The run drew while the hold was pending, and the brain's `Cancelled` ended the hold.
    let (watching, deliver, at) = thinking(cx);
    draw(&watching, RUN, OTHER, cx);
    told(&watching);
    press_deferred(&deliver, at, cx);
    status(&watching.ready.owner, 1, 1, HoldStatus::Cancelled, cx);
    cx.run_until_parked();
    let stop = stopped(vec![cut(OTHER)]);
    assert_eq!(told(&watching), [cancel(), Told::Stop, cancel(), stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.presenter.stoppable_run(), None);
        assert_eq!(scene_ids(&owner.presenter), [OTHER, DRAWN]);
    });
}

#[gpui::test]
fn a_deferred_press_stops_no_other_run_once_its_answer_ended(cx: &mut TestAppContext) {
    let (watching, deliver, at) = thinking(cx);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    state(&watching, 8, Phase::Answering, None, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), Some(RunId(8)));
    });
    cx.run_until_parked();
    assert_eq!(told(&watching), [cancel(), Told::Stop]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), Some(RunId(8)), "run 8 goes on");
        assert_eq!(scene_ids(&owner.presenter), [DRAWN]);
    });
}

#[gpui::test]
fn a_deferred_press_whose_hold_became_an_answer_first_stops_that_answer(cx: &mut TestAppContext) {
    let (watching, deliver, at) = thinking(cx);
    press_deferred(&deliver, at, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.pending.is_none(), "the answer ended the hold");
        assert_eq!(owner.runs.stoppable(), Some(RunId(RUN)));
    });
    cx.run_until_parked();
    // The sources stop, and the press stops the answer, which has drawn nothing to cut.
    assert_eq!(told(&watching), [cancel(), Told::Stop, stopped(Vec::new())]);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), None, "the answer is stopped");
        assert_eq!(scene_ids(&owner.presenter), [DRAWN]);
    });
}

#[gpui::test]
fn a_cancel_marker_left_behind_takes_no_other_press_and_cancels_no_other_hold(
    cx: &mut TestAppContext,
) {
    let clear = || {
        Told::Sent(ControlToBrain::ClearDrawing {
            scene_ids: vec![DRAWN],
        })
    };

    // A press that names a hold which was never pending, as a refused one's does, is an
    // ordinary one.
    let watching = watch(true, cx);
    drawn(&watching, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        owner.native_escape(Some(GestureId(1)), cx).expect("escape");
    });
    cx.run_until_parked();
    assert_eq!(told(&watching), [clear()]);

    // The hotkey's press never came. The next press names no hold and is an ordinary one.
    let watching = watch(true, cx);
    drawn(&watching, cx);
    let _native = ended_by_esc_down(&watching, cx);
    told(&watching);
    watching.ready.owner.update(cx, |owner, cx| {
        owner.native_escape(None, cx).expect("escape");
        assert_eq!(owner.voice.ended, None);
    });
    cx.run_until_parked();
    assert_eq!(told(&watching), [clear()]);

    // A newer hold's own press cancels that hold: hold 1's marker is not its.
    let watching = watch(true, cx);
    drawn(&watching, cx);
    let _native = ended_by_esc_down(&watching, cx);
    hold(&watching, 2, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        owner.native_escape(Some(GestureId(2)), cx).expect("escape");
        assert!(owner.voice.pending.is_none(), "hold 2 is cancelled");
    });

    // And hold 1's marker, whatever brings it back, leaves hold 2 pending.
    let watching = watch(true, cx);
    drawn(&watching, cx);
    let _native = ended_by_esc_down(&watching, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
    });
    hold(&watching, 2, cx);
    watching.ready.owner.update(cx, |owner, cx| {
        owner.native_escape(Some(GestureId(1)), cx).expect("escape");
        let hold = owner.voice.pending.as_ref().expect("hold 2");
        assert_eq!(hold.token.gesture(), GestureId(2));
    });

    // A lost session forgets the marker.
    let watching = watch(true, cx);
    let _native = ended_by_esc_down(&watching, cx);
    let lost = LinkEvent::State(LinkState::Reconnecting);
    receive(&watching.ready.owner, lost, cx);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.ended, None);
    });
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
fn marks_of_an_answer_a_newer_run_replaced_are_timed_from_its_own_end(cx: &mut TestAppContext) {
    let before = Instant::now();
    let watching = marks_up(cx);
    // A newer run is named while the marks' answer still comes.
    state(&watching, RUN + 1, Phase::Thinking, None, cx);
    assert_eq!(watchdog(&watching, cx), None, "the answer hasn't ended");
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    let limit = watchdog(&watching, cx).expect("armed at the answer's end");
    let kept = Duration::from_secs(60);
    assert!(limit >= before + kept && limit <= Instant::now() + kept);
    // A duplicate of either ending leaves that minute as it is, and the newer run goes on.
    state(&watching, RUN, Phase::Done, None, cx);
    end(&watching, RUN, cx);
    assert_eq!(watchdog(&watching, cx), Some(limit));
    assert_eq!(told(&watching), []);
    watching.ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.runs.stoppable(), Some(RunId(RUN + 1)));
    });

    // Marks that were only expected: the watch ends at that run's end instead.
    let watching = released(true, cx);
    state(&watching, RUN, Phase::Thinking, HOLD, cx);
    state(&watching, RUN, Phase::Answering, None, cx);
    state(&watching, RUN + 1, Phase::Thinking, None, cx);
    state(&watching, RUN, Phase::Done, None, cx);
    assert_eq!(sources(&watching), [], "one ending is in");
    end(&watching, RUN, cx);
    assert_eq!(sources(&watching), [Told::Stop]);
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
