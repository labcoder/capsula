//! A hold shows the screen only under the standing notice: nothing is taken at recognition, the
//! pointer's line follows the keys, and the release takes the lease, its stamp and one send.

#![expect(
    clippy::as_conversions,
    reason = "fixed slot discriminants index the fixture's eight surfaces"
)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::geometry::{PhysicalPoint, PointerPath, ReleasePointer};
use kumochi_protocol::message::{
    CaptureFailure, CaptureFor, ConnectionKind, ControlToBrain, DataToBrain, DataToShell, Effort,
    GestureId, HeldScreen, HoldFailure, HoldStatus, LeaseGeneration, ModelChoice, ModelInfo,
    ModelName, ModelSlug, PageOf, ScreenClass, ScreenHolds, ScreenOff, ThreadId, ThreadKind,
    ThreadRow,
};
use kumochi_shell_core::presence::exclusion::machine::fixture::{EXCLUDED, Fake, PRIOR};

use super::exclusion::{Error, Slot};
use super::overlay_visibility::Visibility;
use super::voice_wiring_tests::{ready, status};
use super::{Presence, voice::Stamp};
use crate::input::TalkHeld;
use crate::input::hold::Intent;
use crate::link::{Batch, LinkEvent, LinkState};
use crate::platform::cursor_fixture;
use crate::runs::ScreenNotice;
use crate::strings::Text;

/// Where the pointer is when a hold is recognized, and two places it goes: each far past the
/// dead zone and the spacing at any scale, and on the test display.
const ORIGIN: PhysicalPoint = PhysicalPoint([640, 320]);
const ACROSS: PhysicalPoint = PhysicalPoint([900, 320]);
pub(super) const DOWN: PhysicalPoint = PhysicalPoint([900, 600]);
const QPC: u64 = 77;
const RELEASED: u64 = 4_077;
const STAMP: u64 = 4_242;
/// The four surfaces the fixture owns, covered.
const COVERED: [u32; 8] = [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32, 64];

/// An entry of the `ChatGPT` catalog as the brain's account fixtures list it.
fn model(slug: &str, name: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).unwrap(),
        display_name: ModelName::new(name).unwrap(),
        supported_efforts: None,
    }
}
pub(super) fn luna() -> ModelInfo {
    model("gpt-6-luna", "GPT-6 Luna")
}
fn sol() -> ModelInfo {
    model("gpt-6-sol", "GPT-6 Sol")
}
fn choice(model: &ModelInfo, effort: Effort) -> ModelChoice {
    ModelChoice {
        model: model.slug.clone(),
        effort,
        connection: ConnectionKind::ChatGpt,
    }
}
/// The offer naming `model` at `revision`.
fn offer(model: &ModelInfo, revision: kumochi_protocol::message::NoticeRevision) -> ScreenHolds {
    ScreenHolds::On {
        model: model.display_name.clone(),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: model.slug.clone(),
        revision,
    }
}
pub(super) fn on(model: &ModelInfo) -> ScreenHolds {
    ScreenHolds::On {
        model: model.display_name.clone(),
        connection: kumochi_protocol::message::ConnectionKind::ChatGpt,
        slug: model.slug.clone(),
        revision: kumochi_protocol::message::NoticeRevision::FIRST,
    }
}

pub(super) struct Ready {
    pub(super) owner: Entity<Presence>,
    fixture: Rc<RefCell<Fake>>,
    pub(super) controls: async_channel::Receiver<ControlToBrain>,
    _data: async_channel::Receiver<DataToBrain>,
}

pub(super) fn receive(owner: &Entity<Presence>, event: LinkEvent, cx: &mut TestAppContext) {
    owner
        .update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx)
        })
        .expect("link batch");
}

fn gate(ready: &Ready, state: ScreenHolds, cx: &mut TestAppContext) {
    receive(
        &ready.owner,
        LinkEvent::ScreenHolds {
            generation: 1,
            state,
        },
        cx,
    );
}

/// A connected presence whose default model is Luna, with the brain's state stored. Its stamp
/// reads STAMP, and only once every surface the fixture owns is covered.
pub(super) fn screen_ready(state: Option<ScreenHolds>, cx: &mut TestAppContext) -> Ready {
    let (owner, data, controls) = ready(cx);
    cursor_fixture::set(Some(ORIGIN.0));
    let fixture = Rc::new(RefCell::new(Fake::new()));
    let seen = Rc::clone(&fixture);
    owner.update(cx, |owner, cx| {
        owner.exclusion_fixture = Some(Rc::clone(&fixture));
        owner.counter_fixture = Some(Rc::new(move || {
            assert_eq!(
                seen.borrow().affinities(),
                COVERED,
                "the stamp is read after the last surface is covered"
            );
            i64::try_from(STAMP).unwrap()
        }));
        owner.settings.update(cx, |settings, _| {
            settings.set_models(ConnectionKind::ChatGpt, vec![sol(), luna()]);
            let mut view = settings.view().unwrap().clone();
            view.default_model = Some(choice(&luna(), Effort::Low));
            settings.set_view(view, crate::settings::Update::Unsolicited);
        });
        owner.refresh_conversation(cx);
    });
    let ready = Ready {
        owner,
        fixture,
        controls,
        _data: data,
    };
    if let Some(state) = state {
        gate(&ready, state, cx);
    }
    ready
}

/// Delivers one intent as the native callback delivers it: through the dock window's update.
fn deliver(
    owner: &Entity<Presence>,
    intent: Intent,
    qpc: u64,
    pointer: Option<PhysicalPoint>,
    held: TalkHeld,
    cx: &mut TestAppContext,
) {
    deliver_in(owner, 1, intent, qpc, pointer, held, cx);
}

/// As [`deliver`], stamped on connection `session`.
fn deliver_in(
    owner: &Entity<Presence>,
    session: u64,
    intent: Intent,
    qpc: u64,
    pointer: Option<PhysicalPoint>,
    held: TalkHeld,
    cx: &mut TestAppContext,
) {
    let (dock, stamp) = owner.read_with(cx, |owner, _| {
        let stamp = Stamp {
            target: owner.voice.target.get(),
            session,
            choice: owner.link.choice_revision(),
        };
        (owner.dock_window, stamp)
    });
    assert!(super::gesture::deliver_intent(
        &mut cx.to_async(),
        dock,
        &owner.downgrade(),
        intent,
        stamp,
        qpc,
        pointer,
        held,
    ));
}

/// Recognizes a hold whose pointer was read at `pointer`.
fn recognize_at(
    owner: &Entity<Presence>,
    id: u32,
    pointer: Option<PhysicalPoint>,
    cx: &mut TestAppContext,
) {
    recognize_in(owner, id, 1, pointer, cx);
}

/// Recognizes a hold, as the native callback stamps it on connection `session`.
fn recognize_in(
    owner: &Entity<Presence>,
    id: u32,
    session: u64,
    pointer: Option<PhysicalPoint>,
    cx: &mut TestAppContext,
) {
    let gesture = GestureId(id);
    let token = owner.update(cx, |owner, cx| {
        crate::presence::voice_wiring_tests::gate(owner, id, cx);
        let token = owner
            .link
            .microphone_gate()
            .arm(gesture)
            .expect("fresh hold");
        owner.link.track_input(gesture);
        token
    });
    let intent = Intent::Recognized {
        gesture,
        generation: Ok(token),
    };
    deliver_in(
        owner,
        session,
        intent,
        QPC,
        pointer,
        TalkHeld::default(),
        cx,
    );
    // The line's timer counts from here.
    cx.run_until_parked();
}

pub(super) fn recognize(owner: &Entity<Presence>, id: u32, cx: &mut TestAppContext) {
    recognize_at(owner, id, Some(ORIGIN), cx);
}

/// The hold hears the microphone open, as far as its release can tell.
pub(super) fn listening(owner: &Entity<Presence>, cx: &mut TestAppContext) {
    owner.update(cx, |owner, _| {
        owner.voice.status = Some(Text::HoldListening);
        if let Some(hold) = owner.voice.pending.as_mut() {
            hold.collected = true;
        }
    });
}

/// The keys of hold `id` come up with the pointer read at `pointer`.
pub(super) fn release(
    owner: &Entity<Presence>,
    id: u32,
    pointer: Option<PhysicalPoint>,
    cx: &mut TestAppContext,
) {
    release_holding(owner, id, pointer, TalkHeld::default(), cx);
}

/// The first key of hold `id` comes up while `held` are still down.
pub(super) fn release_holding(
    owner: &Entity<Presence>,
    id: u32,
    pointer: Option<PhysicalPoint>,
    held: TalkHeld,
    cx: &mut TestAppContext,
) {
    let intent = Intent::End {
        gesture: GestureId(id),
        cancelled: false,
    };
    deliver(owner, intent, RELEASED, pointer, held, cx);
}

/// A recognized hold that is listening, its recognition message read.
pub(super) fn held(ready: &Ready, id: u32, cx: &mut TestAppContext) {
    recognize(&ready.owner, id, cx);
    assert_eq!(sent(ready), [screen_hold(id)]);
    listening(&ready.owner, cx);
}

/// The pointer is at `at` when the hold's next frame reads it.
pub(super) fn tick(at: PhysicalPoint, cx: &mut TestAppContext) {
    cursor_fixture::set(Some(at.0));
    frame(cx);
}

/// The next frame of every window, a fifth of a second after the one before: long enough for
/// the eased line to have caught the pointer up, so what a frame keeps is where the pointer is.
fn frame(cx: &mut TestAppContext) {
    advance_ms(200, cx);
    for window in cx.windows() {
        window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .expect("an open window");
    }
    cx.run_until_parked();
}

/// The points of the hold's line so far; none while no hold is sampling.
fn sampled(ready: &Ready, cx: &mut TestAppContext) -> Option<usize> {
    ready.owner.read_with(cx, |owner, _| {
        owner.ink.borrow().as_ref().map(|ink| ink.line().len())
    })
}

fn voice_only(id: u32) -> ControlToBrain {
    ControlToBrain::VoiceHoldRecognized {
        gesture: GestureId(id),
        thread: Some(ThreadId(1)),
        selection: 1,
    }
}

pub(super) fn screen_hold(id: u32) -> ControlToBrain {
    ControlToBrain::ScreenHoldRecognized {
        gesture: GestureId(id),
        thread: Some(ThreadId(1)),
        selection: 1,
        t_qpc: QPC,
        notice: Some(kumochi_protocol::message::NoticeRevision::FIRST),
    }
}

pub(super) fn screen_release(
    id: u32,
    lease: u32,
    pointer: PhysicalPoint,
    path: &[PhysicalPoint],
) -> ControlToBrain {
    ControlToBrain::ScreenRelease {
        gesture: GestureId(id),
        lease: LeaseGeneration(lease),
        t_qpc: RELEASED,
        excluded_qpc: STAMP,
        pointer: ReleasePointer(pointer),
        path: PointerPath(path.to_vec()),
        notice: Some(kumochi_protocol::message::NoticeRevision::FIRST),
    }
}

fn stopped(id: u32) -> [ControlToBrain; 2] {
    [
        ControlToBrain::GestureCancel {
            gesture: GestureId(id),
        },
        ControlToBrain::VoiceHoldStopped {
            gesture: GestureId(id),
        },
    ]
}

fn sent(ready: &Ready) -> Vec<ControlToBrain> {
    std::iter::from_fn(|| ready.controls.try_recv().ok()).collect()
}

/// Ends hold `id` before its run, the way a refused admission does, and lets the next one in.
fn finish(ready: &Ready, id: u32, cx: &mut TestAppContext) {
    status(
        &ready.owner,
        1,
        id,
        HoldStatus::Failed(HoldFailure::Busy),
        cx,
    );
    cx.run_until_parked();
    ready.owner.read_with(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(id));
    });
    assert_eq!(sent(ready), stopped(id));
}

fn advance_ms(millis: u64, cx: &mut TestAppContext) {
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_millis(millis));
    cx.run_until_parked();
}

fn advance(seconds: u64, cx: &mut TestAppContext) {
    advance_ms(seconds * 1_000, cx);
}

/// What a hold on the current target does with the screen now, by the notice's name.
fn notice(ready: &Ready, cx: &mut TestAppContext) -> &'static str {
    ready
        .owner
        .read_with(cx, |owner, cx| match owner.screen_notice(cx) {
            ScreenNotice::Hidden => "hidden",
            ScreenNotice::On(_) => "on",
            ScreenNotice::ModelCantSee(_) => "cant see",
            ScreenNotice::NotReady => "not ready",
            ScreenNotice::TurnedOff => "off",
            ScreenNotice::OwnModel => "own model",
        })
}

pub(super) fn line(ready: &Ready, cx: &mut TestAppContext) -> Option<(Text, Option<ModelName>)> {
    ready
        .owner
        .read_with(cx, |owner, cx| owner.panel.read(cx).screen_line.clone())
}

/// The newest lease's generation, and whether it is still in force.
fn lease(ready: &Ready, cx: &mut TestAppContext) -> (u32, bool) {
    ready.owner.read_with(cx, |owner, _| {
        (
            owner.exclusion.generation(),
            owner.exclusion.lease().is_some(),
        )
    })
}

/// Makes `model` the default, as the brain's settings say it.
fn default_model(ready: &Ready, model: &ModelInfo, cx: &mut TestAppContext) {
    let mut view = ready.owner.read_with(cx, |owner, cx| {
        owner.settings.read(cx).view().unwrap().clone()
    });
    view.default_model = Some(choice(model, Effort::Low));
    receive(
        &ready.owner,
        LinkEvent::Settings {
            generation: 1,
            view,
            answer: false,
        },
        cx,
    );
}

/// Reads the main thread's row, stored with this choice, the way a list page delivers it.
fn read_row(ready: &Ready, choice: Option<ModelChoice>, cx: &mut TestAppContext) {
    let request = ready.owner.update(cx, |owner, cx| {
        owner.threads.update(cx, |state, _| {
            state.connected();
            state.show();
            state
                .requests()
                .into_iter()
                .find_map(|request| match request {
                    DataToBrain::PageRequest {
                        request,
                        page: PageOf::Threads { .. },
                    } => Some(request),
                    _ => None,
                })
                .expect("a list request")
        })
    });
    receive(
        &ready.owner,
        LinkEvent::Data {
            generation: 1,
            bytes: 0,
            message: DataToShell::ThreadList {
                request,
                threads: vec![ThreadRow {
                    id: ThreadId(1),
                    title: String::new(),
                    updated_ms: 0,
                    kind: ThreadKind::Main,
                    choice: choice.clone(),
                }],
                next: None,
            },
        },
        cx,
    );
    ready.owner.read_with(cx, |owner, cx| {
        assert_eq!(
            owner
                .threads
                .read(cx)
                .row(ThreadId(1))
                .map(|row| row.choice.clone()),
            Some(choice)
        );
    });
}

#[gpui::test]
fn a_hold_shows_the_screen_only_while_the_stored_state_is_on(cx: &mut TestAppContext) {
    for (state, said) in [
        (None, "not ready"),
        (Some(ScreenHolds::Off(ScreenOff::Unsupported)), "hidden"),
        (Some(ScreenHolds::Off(ScreenOff::ModelCantSee)), "cant see"),
        (Some(ScreenHolds::Off(ScreenOff::NotReady)), "not ready"),
        (Some(ScreenHolds::Off(ScreenOff::TurnedOff)), "off"),
    ] {
        let ready = screen_ready(state, cx);
        assert_eq!(notice(&ready, cx), said);
        recognize(&ready.owner, 1, cx);
        assert_eq!(sent(&ready), [voice_only(1)]);
        listening(&ready.owner, cx);
        // A voice hold samples nothing, however the pointer moves, and releases nothing.
        tick(ACROSS, cx);
        assert_eq!(sampled(&ready, cx), None);
        release(&ready.owner, 1, Some(DOWN), cx);
        assert_eq!(sent(&ready), []);
        assert_eq!(line(&ready, cx), None);
        assert_eq!(lease(&ready, cx), (0, false));
        assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    }

    // The first run: the hold says nothing about the screen.
    let ready = screen_ready(Some(on(&luna())), cx);
    assert_eq!(notice(&ready, cx), "on");
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);
    assert_eq!(line(&ready, cx), None);
    // Nothing of the screen is taken at recognition.
    assert_eq!(lease(&ready, cx), (0, false));
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    assert_eq!(sampled(&ready, cx), Some(0));
}

#[gpui::test]
fn the_line_follows_the_pointer_and_one_release_takes_the_lease_its_stamp_and_the_path(
    cx: &mut TestAppContext,
) {
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    // Inside the dead zone nothing is drawn; past it the line starts where the pointer was.
    tick(PhysicalPoint([650, 320]), cx);
    assert_eq!(sampled(&ready, cx), Some(0));
    tick(ACROSS, cx);
    assert_eq!(sampled(&ready, cx), Some(2));
    assert_eq!(sent(&ready), [], "first points");
    // A frame that finds the pointer where it was adds nothing, and one that can't read it
    // is skipped.
    frame(cx);
    assert_eq!(sent(&ready), [], "same place");
    cursor_fixture::set(None);
    frame(cx);
    assert_eq!(sampled(&ready, cx), Some(2));
    assert_eq!(sent(&ready), [], "unread");
    tick(DOWN, cx);
    assert_eq!(sampled(&ready, cx), Some(3));
    assert_eq!(sent(&ready), []);
    assert_eq!(lease(&ready, cx), (0, false));

    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(
        sent(&ready),
        [screen_release(1, 1, DOWN, &[ORIGIN, ACROSS, DOWN])]
    );
    assert_eq!(sampled(&ready, cx), None, "the line stops at release");
    assert_eq!(lease(&ready, cx), (1, true));
    assert_eq!(ready.fixture.borrow().affinities(), COVERED);
    assert_eq!(line(&ready, cx), None);

    // The gesture asks once: a duplicate release neither cancels the first nor takes a lease.
    release(&ready.owner, 1, Some(ACROSS), cx);
    tick(ORIGIN, cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(sampled(&ready, cx), None);
    assert_eq!(lease(&ready, cx), (1, true));
}

#[gpui::test]
fn a_notice_that_goes_off_while_the_keys_are_down_ends_the_line_and_the_release_for_good(
    cx: &mut TestAppContext,
) {
    type Change = fn(&Ready, &mut TestAppContext);
    let changes: [(Change, Text); 3] = [
        (
            |ready, cx| gate(ready, ScreenHolds::Off(ScreenOff::TurnedOff), cx),
            Text::ScreenNotIncludedTurnedOff,
        ),
        // The default model changes, so the stored state names the one before it.
        (
            |ready, cx| default_model(ready, &sol(), cx),
            Text::ScreenNotIncludedNotReady,
        ),
        // The thread gets its own model.
        (
            |ready, cx| read_row(ready, Some(choice(&sol(), Effort::Low)), cx),
            Text::ScreenNotIncludedNotReady,
        ),
    ];
    for (change, said) in changes {
        let ready = screen_ready(Some(on(&luna())), cx);
        held(&ready, 1, cx);
        tick(ACROSS, cx);
        assert_eq!(sampled(&ready, cx), Some(2));
        change(&ready, cx);
        assert_eq!(sampled(&ready, cx), None, "{said:?}");
        assert_eq!(line(&ready, cx), Some((said, None)));
        // The keys are still down, and the line's frames end with it all the same.
        frame(cx);
        frame(cx);
        let waiting: usize = (cx.windows().into_iter())
            .map(|window| frames_of(window, cx))
            .sum();
        assert_eq!(waiting, 0, "{said:?}");
        // Back on before the keys come up: the refusal was the gesture's one ask.
        default_model(&ready, &luna(), cx);
        read_row(&ready, None, cx);
        gate(&ready, on(&luna()), cx);
        assert_eq!(notice(&ready, cx), "on");
        tick(DOWN, cx);
        assert_eq!(sampled(&ready, cx), None, "{said:?}");
        assert_eq!(sent(&ready), [], "{said:?}");
        release(&ready.owner, 1, Some(DOWN), cx);
        assert_eq!(sent(&ready), [], "{said:?}");
        assert_eq!(lease(&ready, cx), (0, false), "{said:?}");
        assert_eq!(line(&ready, cx), Some((said, None)));
    }
}

#[gpui::test]
fn a_release_that_cant_take_what_its_capture_needs_sends_nothing_and_says_so(
    cx: &mut TestAppContext,
) {
    let not_ready = Some((Text::ScreenNotIncludedNotReady, None));

    // A surface that can't be covered.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    ready.fixture.borrow_mut().set_failure = Some(Slot::Dock as usize);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(line(&ready, cx), not_ready);
    assert_eq!(lease(&ready, cx), (1, false));
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    advance(3, cx);
    assert_eq!(
        sent(&ready),
        [],
        "a lease the brain never saw tells no lapse"
    );

    // The pointer couldn't be read, so nothing picks the display and no lease is asked for.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    release(&ready.owner, 1, None, cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(line(&ready, cx), not_ready);
    assert_eq!(lease(&ready, cx), (0, false));

    // The notice went off with no word from the brain, as when the shell has queued a new
    // default: the release asks before it takes anything.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    ready.owner.update(cx, |owner, cx| {
        owner.settings.update(cx, |settings, _| {
            let mut view = settings.view().unwrap().clone();
            view.default_model = Some(choice(&sol(), Effort::Low));
            settings.set_view(view, crate::settings::Update::Unsolicited);
        });
    });
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(line(&ready, cx), not_ready);
    assert_eq!(lease(&ready, cx), (0, false));

    // The counter gave no stamp, so the lease is given back.
    let ready = screen_ready(Some(on(&luna())), cx);
    ready.owner.update(cx, |owner, _| {
        owner.counter_fixture = Some(Rc::new(|| 0));
    });
    held(&ready, 1, cx);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(line(&ready, cx), not_ready);
    assert_eq!(lease(&ready, cx), (1, false));
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);

    // The capture before it still holds its lease.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), [screen_release(1, 1, DOWN, &[])]);
    finish(&ready, 1, cx);
    held(&ready, 2, cx);
    release(&ready.owner, 2, Some(DOWN), cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(line(&ready, cx), not_ready);
    assert_eq!(lease(&ready, cx), (1, true), "the first lease is untouched");
    // The first hold's lease is still the one the shell answers for.
    advance(3, cx);
    assert_eq!(
        sent(&ready),
        [ControlToBrain::ExclusionLapsed {
            lease: LeaseGeneration(1)
        }]
    );
}

#[gpui::test]
fn a_hold_or_a_release_the_brain_never_got_leaves_no_line_and_no_lease(cx: &mut TestAppContext) {
    // The recognition can't be sent: no line starts for a hold the brain never heard of.
    let ready = screen_ready(Some(on(&luna())), cx);
    assert!(ready.controls.close());
    recognize(&ready.owner, 1, cx);
    tick(ACROSS, cx);
    assert_eq!(sampled(&ready, cx), None);
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
    });

    // The release can't be sent: its lease is given back at once, and that is no lapse.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    assert!(ready.controls.close());
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(lease(&ready, cx), (1, false));
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.screen_lease, None);
    });
}

#[gpui::test]
fn a_release_off_the_line_s_display_or_with_no_line_carries_an_empty_path(cx: &mut TestAppContext) {
    // Released on a display left of the one the line was drawn on.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    tick(ACROSS, cx);
    assert_eq!(sampled(&ready, cx), Some(2));
    let elsewhere = PhysicalPoint([-200, 320]);
    release(&ready.owner, 1, Some(elsewhere), cx);
    assert_eq!(sent(&ready), [screen_release(1, 1, elsewhere, &[])]);

    // The pointer never left the dead zone.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    tick(PhysicalPoint([650, 325]), cx);
    release(&ready.owner, 1, Some(ORIGIN), cx);
    assert_eq!(sent(&ready), [screen_release(1, 1, ORIGIN, &[])]);

    // The pointer couldn't be read at recognition: no line, and the picture goes alone.
    let ready = screen_ready(Some(on(&luna())), cx);
    recognize_at(&ready.owner, 1, None, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);
    listening(&ready.owner, cx);
    tick(ACROSS, cx);
    assert_eq!(sampled(&ready, cx), None);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), [screen_release(1, 1, DOWN, &[])]);
}

#[gpui::test]
fn a_hold_that_ends_before_its_release_stops_the_line_and_takes_no_lease(cx: &mut TestAppContext) {
    type End = fn(&Ready, &mut TestAppContext);
    let ends: [End; 4] = [
        // The keys come up cancelled.
        |ready, cx| {
            let intent = Intent::End {
                gesture: GestureId(1),
                cancelled: true,
            };
            deliver(
                &ready.owner,
                intent,
                RELEASED,
                Some(DOWN),
                TalkHeld::default(),
                cx,
            );
        },
        // The keys come up before the hold is listening.
        |ready, cx| {
            ready.owner.update(cx, |owner, _| {
                owner.voice.status = Some(Text::HoldPreparing);
                owner.voice.pending.as_mut().expect("hold 1").collected = false;
            });
            release(&ready.owner, 1, Some(DOWN), cx);
        },
        // Esc.
        |ready, cx| {
            ready
                .owner
                .update(cx, |owner, cx| owner.native_escape(Some(GestureId(1)), cx))
                .expect("escape");
        },
        // The brain fails the hold.
        |ready, cx| {
            status(
                &ready.owner,
                1,
                1,
                HoldStatus::Failed(HoldFailure::Busy),
                cx,
            );
        },
    ];
    for (index, end) in ends.into_iter().enumerate() {
        let ready = screen_ready(Some(on(&luna())), cx);
        held(&ready, 1, cx);
        tick(ACROSS, cx);
        assert_eq!(sampled(&ready, cx), Some(2), "{index}");
        end(&ready, cx);
        assert_eq!(sampled(&ready, cx), None, "{index}");
        cx.run_until_parked();
        while ready.controls.try_recv().is_ok() {}
        tick(DOWN, cx);
        assert_eq!(sampled(&ready, cx), None, "{index}");
        assert_eq!(lease(&ready, cx), (0, false), "{index}");
        assert_eq!(sent(&ready), [], "{index}");
    }

    // A hold dropped without its end: the next tick finds no pending hold and clears the line.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    tick(ACROSS, cx);
    ready.owner.update(cx, |owner, _| {
        owner.voice.pending = None;
    });
    assert_eq!(sampled(&ready, cx), Some(2));
    tick(DOWN, cx);
    assert_eq!(sampled(&ready, cx), None);
}

#[gpui::test]
fn the_target_threads_stored_model_decides_once_its_row_is_read(cx: &mut TestAppContext) {
    let shown = "on";
    // Never read: counted as on the default model.
    let ready = screen_ready(Some(on(&luna())), cx);
    ready.owner.read_with(cx, |owner, cx| {
        assert!(owner.threads.read(cx).row(ThreadId(1)).is_none());
    });
    assert_eq!(notice(&ready, cx), shown);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);

    // Read, and on the default model: no stored choice, or the default at another effort.
    for stored in [None, Some(choice(&luna(), Effort::High))] {
        let ready = screen_ready(Some(on(&luna())), cx);
        read_row(&ready, stored, cx);
        assert_eq!(notice(&ready, cx), shown);
        recognize(&ready.owner, 1, cx);
        assert_eq!(sent(&ready), [screen_hold(1)]);
    }

    // Read, and pinned to another model, one that could see the screen too: a hold that is voice
    // only.
    let ready = screen_ready(Some(on(&luna())), cx);
    read_row(&ready, Some(choice(&sol(), Effort::Low)), cx);
    assert_eq!(notice(&ready, cx), "own model");
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [voice_only(1)]);
    assert_eq!(sampled(&ready, cx), None);
}

#[gpui::test]
fn an_on_state_naming_another_model_than_the_newest_default_is_not_ready(cx: &mut TestAppContext) {
    // The default is Luna; the brain's last word still names Sol.
    let ready = screen_ready(Some(on(&sol())), cx);
    assert_eq!(notice(&ready, cx), "not ready");
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [voice_only(1)]);
    finish(&ready, 1, cx);

    // The brain catches up with the default, and the next hold shows Luna the screen.
    gate(&ready, on(&luna()), cx);
    assert_eq!(notice(&ready, cx), "on");
    recognize(&ready.owner, 2, cx);
    assert_eq!(sent(&ready), [screen_hold(2)]);
}

#[gpui::test]
fn the_holds_line_follows_the_brains_refusal_and_a_disconnect_clears_the_notice(
    cx: &mut TestAppContext,
) {
    for (outcome, shown) in [
        (
            HeldScreen::NotIncluded(ScreenClass::Changed),
            Some((Text::ShowScreenChanged, None)),
        ),
        (
            HeldScreen::NotIncluded(ScreenClass::TooLarge),
            Some((Text::ScreenNotIncludedPictureTooLarge, None)),
        ),
        (
            HeldScreen::NotIncluded(ScreenClass::ModelCantSee),
            Some((Text::ScreenNotIncludedModel, None)),
        ),
        (HeldScreen::Screen, None),
    ] {
        let ready = screen_ready(Some(on(&luna())), cx);
        held(&ready, 1, cx);
        release(&ready.owner, 1, Some(DOWN), cx);
        assert_eq!(line(&ready, cx), None);
        receive(
            &ready.owner,
            LinkEvent::HoldScreen {
                generation: 1,
                gesture: GestureId(1),
                outcome,
            },
            cx,
        );
        assert_eq!(line(&ready, cx), shown);
        receive(&ready.owner, LinkEvent::State(LinkState::Reconnecting), cx);
        assert_eq!(notice(&ready, cx), "not ready");
        assert_eq!(line(&ready, cx), None);
    }
}

#[gpui::test]
fn a_lease_that_stops_protecting_a_sent_release_tells_the_brain_once(cx: &mut TestAppContext) {
    let lapsed = [ControlToBrain::ExclusionLapsed {
        lease: LeaseGeneration(1),
    }];
    let released = |cx: &mut TestAppContext| {
        let ready = screen_ready(Some(on(&luna())), cx);
        held(&ready, 1, cx);
        release(&ready.owner, 1, Some(DOWN), cx);
        assert_eq!(sent(&ready), [screen_release(1, 1, DOWN, &[])]);
        ready
    };

    // Its timer.
    let ready = released(cx);
    advance(3, cx);
    assert_eq!(sent(&ready), lapsed);
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // A surface that can't be covered.
    let ready = released(cx);
    ready.owner.update(cx, |owner, cx| {
        assert_eq!(
            owner.protect_surface(Slot::Settings, cx),
            Err(Error::Surface)
        );
    });
    assert_eq!(sent(&ready), lapsed);
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // A revoke that leaves restoration debt, and so keeps the lease.
    let ready = released(cx);
    ready.fixture.borrow_mut().restore_failure = 1 << Slot::Proxy as usize;
    ready.owner.update(cx, Presence::revoke_exclusion);
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.exclusion.lease().unwrap().revoked());
    });
    assert_eq!(sent(&ready), lapsed);
    ready.fixture.borrow_mut().restore_failure = 0;
    ready.owner.update(cx, Presence::revoke_exclusion);
    assert_eq!(lease(&ready, cx), (1, false));
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // A cancel after the release leaves the lease to its capture.
    let ready = released(cx);
    finish(&ready, 1, cx);
    assert_eq!(lease(&ready, cx), (1, true));
    assert_eq!(ready.fixture.borrow().affinities(), COVERED);
}

#[gpui::test]
fn a_capture_answer_releases_its_own_lease_without_a_lapse_and_never_a_newer_one(
    cx: &mut TestAppContext,
) {
    let done = |gesture, ready: &Ready, cx: &mut TestAppContext| {
        receive(
            &ready.owner,
            LinkEvent::CaptureDone {
                generation: 1,
                capture_for: CaptureFor::Gesture(GestureId(gesture)),
            },
            cx,
        );
    };
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), [screen_release(1, 1, DOWN, &[])]);
    done(1, &ready, cx);
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // The first hold's lease lapses; its capture answers only after a second release has one.
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    release(&ready.owner, 1, Some(DOWN), cx);
    advance(3, cx);
    assert_eq!(
        sent(&ready),
        [
            screen_release(1, 1, DOWN, &[]),
            ControlToBrain::ExclusionLapsed {
                lease: LeaseGeneration(1)
            }
        ]
    );
    finish(&ready, 1, cx);
    held(&ready, 2, cx);
    release(&ready.owner, 2, Some(ACROSS), cx);
    // The second release's lease is the second generation, never the one before it.
    assert_eq!(sent(&ready), [screen_release(2, 2, ACROSS, &[])]);
    done(1, &ready, cx);
    receive(
        &ready.owner,
        LinkEvent::CaptureFailed {
            generation: 1,
            capture_for: CaptureFor::Gesture(GestureId(1)),
            reason: CaptureFailure::Cancelled,
        },
        cx,
    );
    assert_eq!(ready.fixture.borrow().affinities(), COVERED);
    assert_eq!(sent(&ready), []);
    // The newer lease is still the one the shell answers for.
    advance(3, cx);
    assert_eq!(
        sent(&ready),
        [ControlToBrain::ExclusionLapsed {
            lease: LeaseGeneration(2)
        }]
    );
}

/// An overlay window of the owner's, opened hidden as the real one is.
pub(super) fn detached(ready: &Ready, cx: &mut TestAppContext) -> super::overlay::Overlay {
    ready.owner.update(cx, |owner, cx| {
        super::overlay::Overlay::detached(
            owner.presenter.frames(),
            cx.weak_entity(),
            Rc::clone(&owner.paint_scratch),
            Rc::clone(&owner.paint_notice),
            Rc::clone(&owner.ink),
            cx,
        )
    })
}

/// Runs the frame callbacks `window` has waiting, and says how many there were.
fn frames_of(window: gpui_kit::AnyWindowHandle, cx: &mut TestAppContext) -> usize {
    let ran = window
        .update(cx, |_, window, cx| window.simulate_next_frame(cx))
        .expect("an open window");
    cx.run_until_parked();
    ran
}

#[gpui::test]
fn the_pointer_is_read_on_frames_the_dock_s_until_the_overlay_shows_and_then_its_own(
    cx: &mut TestAppContext,
) {
    let ready = screen_ready(Some(on(&luna())), cx);
    let hidden = detached(&ready, cx);
    let drawing = hidden.window;
    ready.owner.update(cx, |owner, _| {
        owner.overlay = Visibility::Hidden(hidden);
    });
    let dock = ready.owner.read_with(cx, |owner, _| owner.dock_window);
    assert_eq!(frames_of(dock, cx), 0, "no hold asks for frames");
    held(&ready, 1, cx);
    // Nothing reads the pointer between frames, however long that is.
    cursor_fixture::set(Some(ACROSS.0));
    advance(1, cx);
    assert_eq!(sampled(&ready, cx), Some(0));
    // Windows paints no hidden window, so the dock's frames carry the line until it shows.
    assert_eq!((frames_of(drawing, cx), frames_of(dock, cx)), (0, 1));
    assert_eq!(sampled(&ready, cx), Some(2));
    ready.owner.read_with(cx, |owner, _| {
        assert!(matches!(owner.overlay, Visibility::Shown(_)));
    });
    // That frame asked the dock again before the overlay showed; the next one moves over.
    assert_eq!((frames_of(drawing, cx), frames_of(dock, cx)), (0, 1));
    // From here a frame of the overlay reads the pointer that the same frame paints.
    let repaints = Rc::new(Cell::new(0));
    let _counting = ready.owner.update(cx, |owner, cx| {
        let Visibility::Shown(overlay) = &owner.overlay else {
            panic!("the overlay shows");
        };
        overlay.count_repaints(Rc::clone(&repaints), cx)
    });
    cursor_fixture::set(Some(DOWN.0));
    advance_ms(200, cx);
    assert_eq!((frames_of(dock, cx), frames_of(drawing, cx)), (0, 1));
    assert_eq!((sampled(&ready, cx), repaints.get()), (Some(3), 1));
    assert_eq!(
        (frames_of(dock, cx), frames_of(drawing, cx)),
        (0, 1),
        "one frame asks for the next"
    );
    // A pointer that rests keeps its frames and changes nothing: no point and no repaint.
    assert_eq!((sampled(&ready, cx), repaints.get()), (Some(3), 1));
    advance_ms(200, cx);
    assert_eq!((frames_of(dock, cx), frames_of(drawing, cx)), (0, 1));
    assert_eq!((sampled(&ready, cx), repaints.get()), (Some(3), 1));
}

#[gpui::test]
fn an_ended_hold_s_frames_end_with_it_and_leave_the_next_hold_s_line_alone(
    cx: &mut TestAppContext,
) {
    let ready = screen_ready(Some(on(&luna())), cx);
    let dock = ready.owner.read_with(cx, |owner, _| owner.dock_window);
    held(&ready, 1, cx);
    finish(&ready, 1, cx);
    assert_eq!(sampled(&ready, cx), None);
    // The first hold's frame is still waiting when the second hold asks for its own.
    held(&ready, 2, cx);
    cursor_fixture::set(Some(ACROSS.0));
    assert_eq!(frames_of(dock, cx), 2);
    assert_eq!(sampled(&ready, cx), Some(2), "the second hold's line");
    assert_eq!(frames_of(dock, cx), 1, "only the second hold asked again");
    assert_eq!(sampled(&ready, cx), Some(2));
}

#[gpui::test]
fn the_overlay_shows_for_a_line_alone_and_hides_when_the_line_ends(cx: &mut TestAppContext) {
    let overlay = detached;

    // A hold whose pointer stays put shows nothing; the line's first points show the overlay.
    let ready = screen_ready(Some(on(&luna())), cx);
    let hidden = overlay(&ready, cx);
    ready.owner.update(cx, |owner, _| {
        owner.overlay = Visibility::Hidden(hidden);
    });
    held(&ready, 1, cx);
    tick(PhysicalPoint([650, 320]), cx);
    ready.owner.read_with(cx, |owner, _| {
        assert!(matches!(owner.overlay, Visibility::Hidden(_)));
    });
    tick(ACROSS, cx);
    ready.owner.read_with(cx, |owner, _| {
        assert!(matches!(owner.overlay, Visibility::Shown(_)));
        assert!(owner.presenter.frames().borrow().shown);
    });

    // A line's end hides an overlay that no drawing needs. The hide retires the window it
    // hid, and a test can't open the real one that replaces it.
    let ready = screen_ready(Some(on(&luna())), cx);
    let shown = overlay(&ready, cx);
    ready.owner.update(cx, |owner, _| {
        owner.presenter.frames().borrow_mut().shown = true;
        owner.overlay = Visibility::Shown(shown);
    });
    held(&ready, 1, cx);
    tick(ACROSS, cx);
    tick(DOWN, cx);
    ready.owner.read_with(cx, |owner, _| {
        assert!(matches!(owner.overlay, Visibility::Shown(_)));
    });
    finish(&ready, 1, cx);
    ready.owner.read_with(cx, |owner, _| {
        assert!(matches!(owner.overlay, Visibility::Closed));
        assert!(!owner.presenter.frames().borrow().shown);
    });
}

#[gpui::test]
fn the_peek_shows_a_holds_line_in_place_of_the_settings_notice_and_nothing_else_of_the_screen(
    cx: &mut TestAppContext,
) {
    use gpui_kit::AppContext as _;
    const LINE: &str = "peek-line Screen not included: the display changed";
    const NOTICE: &str = "peek-line Settings not saved";
    let (window, panel) = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(crate::strings::Strings::english());
        let (events, _) = async_channel::unbounded();
        gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |_, cx| {
            let kumo = cx.new(|_| super::KumoView::new());
            cx.new(|_| super::Panel {
                kumo,
                state: super::LinkState::Connected,
                voice: None,
                notice: Some(Text::SettingsNotSaved),
                screen_line: None,
                stop: false,
                events,
                threads: None,
            })
        })
        .expect("a window")
    });
    let drawn = |cx: &mut TestAppContext| {
        cx.run_until_parked();
        let mut frame = gpui_kit::VisualTestContext::from_window(window, cx);
        frame.update(|window, cx| window.draw(cx).clear(cx));
        [LINE, NOTICE].map(|selector| frame.debug_bounds(selector).is_some())
    };
    assert_eq!(drawn(cx), [false, true]);
    // A refusal's line takes the settings notice's place while it shows.
    panel.update(cx, |panel, cx| {
        panel.screen_line = Some((Text::ShowScreenChanged, None));
        cx.notify();
    });
    assert_eq!(drawn(cx), [true, false]);
    panel.update(cx, |panel, cx| {
        panel.screen_line = None;
        cx.notify();
    });
    assert_eq!(drawn(cx), [false, true]);
}

#[gpui::test]
fn a_release_while_the_box_is_unchecked_and_still_saving_sends_nothing_and_says_so(
    cx: &mut TestAppContext,
) {
    let ready = screen_ready(Some(on(&luna())), cx);
    held(&ready, 1, cx);
    // The user unchecks the box. The brain hasn't answered, so its gate still says on.
    assert!(
        ready
            .owner
            .update(cx, |owner, cx| owner.save_show_screen(false, cx))
    );
    assert_eq!(
        sent(&ready),
        [ControlToBrain::Settings(
            kumochi_protocol::message::SettingsOp::SetShowScreen(false)
        )]
    );
    assert_eq!(notice(&ready, cx), "on");
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(
        line(&ready, cx),
        Some((Text::ScreenNotIncludedTurnedOff, None))
    );
    assert_eq!(lease(&ready, cx), (0, false));
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
}

#[gpui::test]
fn a_default_model_that_stops_or_starts_taking_pictures_says_so_once_in_the_open_conversation(
    cx: &mut TestAppContext,
) {
    let ready = screen_ready(Some(on(&luna())), cx);
    let note = |ready: &Ready, cx: &mut TestAppContext| {
        ready.owner.read_with(cx, |owner, cx| {
            let note = owner.chat.read(cx).visible_screen_note().cloned();
            note.map(|note| (note.thread, note.text, note.model))
        })
    };
    let thread = ready.owner.update(cx, |owner, cx| {
        let thread = owner.runs.thread();
        owner.chat.update(cx, |chat, _| {
            chat.open = thread;
            chat.loaded = true;
        });
        thread.expect("a target thread")
    });
    // The first word, that Luna takes pictures, is no change.
    assert_eq!(note(&ready, cx), None);
    gate(&ready, ScreenHolds::Off(ScreenOff::ModelCantSee), cx);
    assert_eq!(
        note(&ready, cx),
        Some((thread, Text::ScreenNoteCant, luna().display_name))
    );
    // Switched off and not ready say nothing and change nothing.
    gate(&ready, ScreenHolds::Off(ScreenOff::TurnedOff), cx);
    gate(&ready, ScreenHolds::Off(ScreenOff::NotReady), cx);
    assert_eq!(
        note(&ready, cx),
        Some((thread, Text::ScreenNoteCant, luna().display_name))
    );
    gate(&ready, on(&luna()), cx);
    assert_eq!(
        note(&ready, cx),
        Some((thread, Text::ScreenNoteCan, luna().display_name))
    );
}

/// A press under one offer, or under none, whose recognition is delivered after a newer offer
/// arrived takes no screen: receiving the newer one doesn't show it.
#[gpui::test]
fn a_hold_carries_no_offer_that_arrived_before_its_deferred_recognition(cx: &mut TestAppContext) {
    use kumochi_protocol::message::NoticeRevision;
    for (id, shown) in [
        (1, on(&luna())),
        (2, ScreenHolds::Off(ScreenOff::ModelCantSee)),
    ] {
        let ready = screen_ready(Some(shown), cx);
        press(&ready, id, cx);
        let warm = Intent::Warm(GestureId(id));
        deliver(
            &ready.owner,
            warm,
            QPC,
            Some(ORIGIN),
            TalkHeld::default(),
            cx,
        );
        gate(&ready, offer(&luna(), NoticeRevision::FIRST.next()), cx);
        recognize(&ready.owner, id, cx);
        listening(&ready.owner, cx);
        release(&ready.owner, id, Some(ORIGIN), cx);
        let sent = sent(&ready);
        assert!(
            !sent
                .iter()
                .any(|message| matches!(message, ControlToBrain::ScreenRelease { .. })),
            "{sent:?}"
        );
        assert!(
            !sent.iter().any(|message| matches!(
                message,
                ControlToBrain::ScreenHoldRecognized {
                    notice: Some(_),
                    ..
                } | ControlToBrain::ScreenRelease {
                    notice: Some(_),
                    ..
                }
            )),
            "{sent:?}"
        );
    }
}

/// Gesture `id`'s first native input, as the callback sees it before anything reaches the dock.
fn press(ready: &Ready, id: u32, cx: &mut TestAppContext) {
    ready.owner.read_with(cx, |owner, cx| {
        crate::presence::voice_wiring_tests::key_down(owner, id, cx);
    });
}

/// The press's Warm, delivered through the dock as the native callback delivers it.
fn warm(ready: &Ready, id: u32, cx: &mut TestAppContext) {
    let intent = Intent::Warm(GestureId(id));
    deliver(
        &ready.owner,
        intent,
        QPC,
        Some(ORIGIN),
        TalkHeld::default(),
        cx,
    );
}

/// `message` carrying the eligibility of offer `revision`.
fn under(
    mut message: ControlToBrain,
    revision: kumochi_protocol::message::NoticeRevision,
) -> ControlToBrain {
    if let ControlToBrain::ScreenHoldRecognized { notice, .. }
    | ControlToBrain::ScreenRelease { notice, .. } = &mut message
    {
        *notice = Some(revision);
    }
    message
}

/// Hold `id` went without the screen: no screen recognition or release, no lease and no
/// marks watch.
fn omitted(ready: &Ready, id: u32, cx: &mut TestAppContext) {
    listening(&ready.owner, cx);
    release(&ready.owner, id, Some(DOWN), cx);
    let sent = sent(ready);
    assert!(
        !sent.iter().any(|message| matches!(
            message,
            ControlToBrain::ScreenHoldRecognized { .. } | ControlToBrain::ScreenRelease { .. }
        )),
        "{sent:?}"
    );
    assert_eq!(lease(ready, cx), (0, false));
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
}

/// The 300 ms race: a press the native callback saw while the screen wasn't offered stays
/// without it when the offer arrives before its Warm is delivered, or after that but before its
/// recognition.
#[gpui::test]
fn a_press_seen_without_an_offer_never_takes_the_screen(cx: &mut TestAppContext) {
    for before_warm in [true, false] {
        let ready = screen_ready(Some(ScreenHolds::Off(ScreenOff::ModelCantSee)), cx);
        press(&ready, 1, cx);
        if before_warm {
            gate(&ready, on(&luna()), cx);
        }
        warm(&ready, 1, cx);
        if !before_warm {
            gate(&ready, on(&luna()), cx);
        }
        recognize(&ready.owner, 1, cx);
        omitted(&ready, 1, cx);
    }
}

/// An offer revised between the press and its recognition leaves the hold without the screen,
/// even for the same model: the new revision may be another account's.
#[gpui::test]
fn a_press_keeps_only_the_offer_it_was_seen_under(cx: &mut TestAppContext) {
    use kumochi_protocol::message::NoticeRevision;
    let ready = screen_ready(Some(on(&luna())), cx);
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    gate(&ready, offer(&luna(), NoticeRevision::FIRST.next()), cx);
    recognize(&ready.owner, 1, cx);
    omitted(&ready, 1, cx);
}

/// A press seen on one connection takes nothing on the next, though that one offers the same
/// model under the revision the first one used.
#[gpui::test]
fn a_press_never_takes_a_later_connections_reused_revision(cx: &mut TestAppContext) {
    let ready = screen_ready(Some(on(&luna())), cx);
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    receive(
        &ready.owner,
        LinkEvent::State(crate::link::LinkState::Reconnecting),
        cx,
    );
    receive(
        &ready.owner,
        LinkEvent::Connected(crate::link::Session::detached(
            2,
            kumochi_protocol::message::Incarnation(1),
        )),
        cx,
    );
    receive(
        &ready.owner,
        LinkEvent::ScreenHolds {
            generation: 2,
            state: on(&luna()),
        },
        cx,
    );
    sent(&ready);
    recognize_in(&ready.owner, 1, 2, Some(ORIGIN), cx);
    omitted(&ready, 1, cx);
}

/// A press seen under an offer that stays the same carries it to its recognition and release.
#[gpui::test]
fn a_press_under_an_unchanged_offer_carries_it_to_its_release(cx: &mut TestAppContext) {
    use kumochi_protocol::message::NoticeRevision;
    let ready = screen_ready(Some(on(&luna())), cx);
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    sent(&ready);
    recognize(&ready.owner, 1, cx);
    listening(&ready.owner, cx);
    assert_eq!(sent(&ready), [under(screen_hold(1), NoticeRevision::FIRST)]);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(
        sent(&ready),
        [under(
            screen_release(1, 1, DOWN, &[]),
            NoticeRevision::FIRST
        )]
    );
    assert_eq!(lease(&ready, cx), (1, true));
}

/// A press seen on one target takes no screen on another: a target chosen after the press and
/// before its recognition leaves the hold voice only, and one chosen after its recognition
/// cancels the hold. Neither takes a lease or a marks watch.
#[gpui::test]
fn a_press_takes_no_screen_on_a_target_it_was_not_seen_on(cx: &mut TestAppContext) {
    for before_recognition in [true, false] {
        let ready = screen_ready(Some(on(&luna())), cx);
        press(&ready, 1, cx);
        let retarget = |cx: &mut TestAppContext| {
            ready.owner.update(cx, Presence::new_thread).unwrap();
        };
        if before_recognition {
            retarget(cx);
        }
        warm(&ready, 1, cx);
        recognize(&ready.owner, 1, cx);
        if !before_recognition {
            retarget(cx);
        }
        listening(&ready.owner, cx);
        release(&ready.owner, 1, Some(DOWN), cx);
        let sent = sent(&ready);
        assert!(
            !sent.iter().any(|message| matches!(
                message,
                ControlToBrain::ScreenRelease { .. }
                    | ControlToBrain::ScreenHoldRecognized { notice: None, .. }
            )),
            "{sent:?}"
        );
        assert_eq!(
            sent.iter()
                .any(|message| matches!(message, ControlToBrain::ScreenHoldRecognized { .. })),
            !before_recognition,
            "{sent:?}"
        );
        assert_eq!(lease(&ready, cx), (0, false));
        ready.owner.read_with(cx, |owner, _| {
            assert!(owner.voice.watched.is_none());
        });
    }
}

/// An offer that goes and comes back is another one, though the brain names it with the same
/// revision: the press under the first takes no screen, whether it went before the recognition
/// or between the recognition and the release.
#[gpui::test]
fn an_offer_that_went_and_came_back_is_not_the_one_the_press_saw(cx: &mut TestAppContext) {
    for before_recognition in [true, false] {
        let ready = screen_ready(Some(on(&luna())), cx);
        press(&ready, 1, cx);
        warm(&ready, 1, cx);
        let away_and_back = |cx: &mut TestAppContext| {
            gate(&ready, ScreenHolds::Off(ScreenOff::NotReady), cx);
            gate(&ready, on(&luna()), cx);
        };
        if before_recognition {
            away_and_back(cx);
        }
        recognize(&ready.owner, 1, cx);
        if !before_recognition {
            away_and_back(cx);
        }
        listening(&ready.owner, cx);
        release(&ready.owner, 1, Some(DOWN), cx);
        let sent = sent(&ready);
        assert!(
            !sent
                .iter()
                .any(|message| matches!(message, ControlToBrain::ScreenRelease { .. })),
            "{sent:?}"
        );
        assert_eq!(lease(&ready, cx), (0, false));
        ready.owner.read_with(cx, |owner, _| {
            assert!(owner.voice.watched.is_none());
        });
    }
}

/// An offer revised for the same model between a hold's recognition and its release leaves the
/// release without the screen, though the notice still says on: no release, lease or watch.
#[gpui::test]
fn a_release_keeps_only_the_offer_its_press_was_seen_under(cx: &mut TestAppContext) {
    use kumochi_protocol::message::NoticeRevision;
    let ready = screen_ready(Some(on(&luna())), cx);
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    sent(&ready);
    recognize(&ready.owner, 1, cx);
    listening(&ready.owner, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);
    gate(&ready, offer(&luna(), NoticeRevision::FIRST.next()), cx);
    assert_eq!(notice(&ready, cx), "on");
    release(&ready.owner, 1, Some(DOWN), cx);
    let sent = sent(&ready);
    assert!(
        !sent
            .iter()
            .any(|message| matches!(message, ControlToBrain::ScreenRelease { .. })),
        "{sent:?}"
    );
    assert_eq!(lease(&ready, cx), (0, false));
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
}

/// The user's Off disqualifies the press it met even when the brain refuses to save it and the
/// choice comes back on; the next press shares under the restored setting.
#[gpui::test]
fn an_off_whose_save_was_refused_keeps_its_press_off_and_the_next_press_shares(
    cx: &mut TestAppContext,
) {
    use kumochi_protocol::message::{ErrorKey, SettingsOp};
    let ready = screen_ready(Some(on(&luna())), cx);
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    sent(&ready);
    recognize(&ready.owner, 1, cx);
    listening(&ready.owner, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);
    assert!(
        ready
            .owner
            .update(cx, |owner, cx| owner.save_show_screen(false, cx))
    );
    assert_eq!(
        sent(&ready),
        [ControlToBrain::Settings(SettingsOp::SetShowScreen(false))]
    );
    let saved = ready.owner.read_with(cx, |owner, cx| {
        owner
            .settings
            .read(cx)
            .view()
            .expect("a saved view")
            .clone()
    });
    assert!(saved.show_screen);
    receive(
        &ready.owner,
        LinkEvent::Error {
            generation: 1,
            error: ErrorKey::SettingsRefused.into(),
        },
        cx,
    );
    receive(
        &ready.owner,
        LinkEvent::Settings {
            generation: 1,
            view: saved,
            answer: true,
        },
        cx,
    );
    ready.owner.read_with(cx, |owner, cx| {
        let state = owner.settings.read(cx);
        assert!(!state.show_screen_pending());
        assert_eq!(state.show_screen_choice(), Some(true));
    });
    assert_eq!(notice(&ready, cx), "on");
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), []);
    assert_eq!(
        line(&ready, cx),
        Some((Text::ScreenNotIncludedNotReady, None))
    );
    assert_eq!(lease(&ready, cx), (0, false));
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
    finish(&ready, 1, cx);

    press(&ready, 2, cx);
    warm(&ready, 2, cx);
    sent(&ready);
    recognize(&ready.owner, 2, cx);
    listening(&ready.owner, cx);
    assert_eq!(sent(&ready), [screen_hold(2)]);
    release(&ready.owner, 2, Some(DOWN), cx);
    assert_eq!(sent(&ready), [screen_release(2, 1, DOWN, &[])]);
    assert_eq!(lease(&ready, cx), (1, true));
    assert_eq!(line(&ready, cx), None);
}

/// Drains `controls`, the recorder a test swapped in.
fn drained(controls: &async_channel::Receiver<ControlToBrain>) -> Vec<ControlToBrain> {
    std::iter::from_fn(|| controls.try_recv().ok()).collect()
}

/// An Off the control queue can't take leaves the saved choice on, but the press it met never
/// takes the screen, even once the queue has room before its release; the next press does.
#[gpui::test]
fn an_off_that_cant_be_queued_keeps_its_press_off_and_the_next_press_shares(
    cx: &mut TestAppContext,
) {
    let ready = screen_ready(Some(on(&luna())), cx);
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    sent(&ready);
    recognize(&ready.owner, 1, cx);
    listening(&ready.owner, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);
    let full = ready.owner.update(cx, |owner, _| {
        let controls = owner.link.record_controls_bounded();
        let session = owner.session.clone().expect("connected");
        while owner.link.send_control(&session, ControlToBrain::Pong) {}
        controls
    });
    assert!(
        !ready
            .owner
            .update(cx, |owner, cx| owner.save_show_screen(false, cx))
    );
    ready.owner.read_with(cx, |owner, cx| {
        let state = owner.settings.read(cx);
        assert!(!state.show_screen_pending());
        assert_eq!(state.show_screen_choice(), Some(true));
        assert!(owner.session.as_ref().expect("connected").open());
    });
    assert_eq!(notice(&ready, cx), "on");
    // The queue has room again before the keys come up.
    assert!(
        drained(&full)
            .iter()
            .all(|message| *message == ControlToBrain::Pong)
    );
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(drained(&full), []);
    assert_eq!(
        line(&ready, cx),
        Some((Text::ScreenNotIncludedNotReady, None))
    );
    assert_eq!(lease(&ready, cx), (0, false));
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.voice.watched.is_none());
    });
    status(
        &ready.owner,
        1,
        1,
        HoldStatus::Failed(HoldFailure::Busy),
        cx,
    );
    cx.run_until_parked();
    ready.owner.read_with(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(1));
    });
    assert_eq!(drained(&full), stopped(1));

    press(&ready, 2, cx);
    warm(&ready, 2, cx);
    drained(&full);
    recognize(&ready.owner, 2, cx);
    listening(&ready.owner, cx);
    assert_eq!(drained(&full), [screen_hold(2)]);
    release(&ready.owner, 2, Some(DOWN), cx);
    assert_eq!(drained(&full), [screen_release(2, 1, DOWN, &[])]);
    assert_eq!(lease(&ready, cx), (1, true));
}

/// A main row read as pinned elsewhere on an earlier connection is no proof once that connection
/// is gone: with Threads closed it doesn't veto the next press, which stays a candidate the brain
/// proves. The same row read on this connection still does (the stored-model test above).
#[gpui::test]
fn a_row_read_before_a_reconnect_does_not_veto_the_next_press(cx: &mut TestAppContext) {
    let ready = screen_ready(Some(on(&luna())), cx);
    read_row(&ready, Some(choice(&sol(), Effort::Low)), cx);
    assert_eq!(notice(&ready, cx), "own model");
    ready.owner.update(cx, |owner, cx| {
        owner.threads.update(cx, |state, _| {
            state.hide();
            state.disconnected();
            state.connected();
        });
        owner.screen_changed(cx);
    });
    ready.owner.read_with(cx, |owner, cx| {
        let threads = owner.threads.read(cx);
        assert!(threads.row(ThreadId(1)).is_some(), "still cached");
        assert!(threads.fresh_row(ThreadId(1)).is_none());
    });
    assert_eq!(notice(&ready, cx), "on");
    press(&ready, 1, cx);
    warm(&ready, 1, cx);
    sent(&ready);
    recognize(&ready.owner, 1, cx);
    listening(&ready.owner, cx);
    assert_eq!(sent(&ready), [screen_hold(1)]);
    release(&ready.owner, 1, Some(DOWN), cx);
    assert_eq!(sent(&ready), [screen_release(1, 1, DOWN, &[])]);
    assert_eq!(lease(&ready, cx), (1, true));
}
