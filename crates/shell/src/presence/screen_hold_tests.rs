//! A hold becomes a screen hold only under the standing notice and a live capture lease.

#![expect(
    clippy::as_conversions,
    reason = "fixed slot discriminants index the fixture's eight surfaces"
)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{Entity, TestAppContext};
use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{
    CaptureFailure, CaptureFor, ConnectionKind, ControlToBrain, DataToBrain, DataToShell, Effort,
    GestureId, HeldScreen, HoldFailure, HoldStatus, LeaseGeneration, ModelChoice, ModelInfo,
    ModelName, ModelSlug, PageOf, ScreenClass, ScreenHolds, ScreenOff, ThreadId, ThreadKind,
    ThreadRow,
};
use kumochi_shell_core::presence::exclusion::machine::fixture::{EXCLUDED, Fake, PRIOR};

use super::exclusion::{Error, Slot};
use super::voice_wiring_tests::{ready, status};
use super::{Presence, voice::Stamp};
use crate::input::hold::Intent;
use crate::link::{Batch, LinkEvent, LinkState};
use crate::strings::Text;

const POINTER: PhysicalPoint = PhysicalPoint([640, -32]);
const QPC: u64 = 77;
/// The four surfaces the fixture owns, covered.
const COVERED: [u32; 8] = [EXCLUDED, EXCLUDED, EXCLUDED, EXCLUDED, 8, 16, 32, 64];

/// An entry of the `ChatGPT` catalog as the brain's account fixtures list it.
fn model(slug: &str, name: &str) -> ModelInfo {
    ModelInfo {
        slug: ModelSlug::new(slug).unwrap(),
        display_name: ModelName::new(name).unwrap(),
    }
}
fn luna() -> ModelInfo {
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
fn on(model: &ModelInfo) -> ScreenHolds {
    ScreenHolds::On {
        model: model.display_name.clone(),
    }
}

struct Ready {
    owner: Entity<Presence>,
    fixture: Rc<RefCell<Fake>>,
    controls: async_channel::Receiver<ControlToBrain>,
    _data: async_channel::Receiver<DataToBrain>,
}

fn receive(owner: &Entity<Presence>, event: LinkEvent, cx: &mut TestAppContext) {
    owner
        .update(cx, |owner, cx| {
            owner.link_events(Batch::Plain(vec![event]), cx)
        })
        .expect("link batch");
}

/// A connected presence whose default model is Luna, with the brain's state stored.
fn screen_ready(state: Option<ScreenHolds>, cx: &mut TestAppContext) -> Ready {
    let (owner, data, controls) = ready(cx);
    let fixture = Rc::new(RefCell::new(Fake::new()));
    owner.update(cx, |owner, cx| {
        owner.exclusion_fixture = Some(Rc::clone(&fixture));
        owner.settings.update(cx, |settings, _| {
            settings.set_models(ConnectionKind::ChatGpt, vec![sol(), luna()]);
            let mut view = settings.view().unwrap().clone();
            view.default_model = Some(choice(&luna(), Effort::Low));
            settings.set_view(view, crate::settings::Update::Unsolicited);
        });
        owner.refresh_conversation(cx);
    });
    if let Some(state) = state {
        receive(
            &owner,
            LinkEvent::ScreenHolds {
                generation: 1,
                state,
            },
            cx,
        );
    }
    Ready {
        owner,
        fixture,
        controls,
        _data: data,
    }
}

/// Recognizes a hold whose pointer was sampled at POINTER.
fn recognize(owner: &Entity<Presence>, id: u32, cx: &mut TestAppContext) {
    owner.update(cx, |owner, cx| {
        let gesture = GestureId(id);
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
            QPC,
            Some(POINTER),
            cx,
        );
    });
}

fn voice_only(id: u32) -> ControlToBrain {
    ControlToBrain::VoiceHoldRecognized {
        gesture: GestureId(id),
        thread: Some(ThreadId(1)),
        selection: 1,
    }
}

fn screen_hold(id: u32, lease: u32) -> ControlToBrain {
    ControlToBrain::HoldRecognized {
        gesture: GestureId(id),
        thread: Some(ThreadId(1)),
        selection: 1,
        pointer: POINTER,
        lease: LeaseGeneration(lease),
        t_qpc: QPC,
    }
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
    ready.owner.read_with(cx, |owner, _| {
        owner.link.accept_recorded_microphone_stop(1, GestureId(id));
    });
    assert_eq!(
        sent(ready),
        [
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(id)
            },
            ControlToBrain::GestureCancel {
                gesture: GestureId(id)
            }
        ]
    );
}

fn advance(seconds: u64, cx: &mut TestAppContext) {
    cx.dispatcher
        .scheduler()
        .clock()
        .advance(Duration::from_secs(seconds));
    cx.run_until_parked();
}

fn icon(ready: &Ready, cx: &mut TestAppContext) -> Option<(Text, Option<ModelName>)> {
    ready
        .owner
        .read_with(cx, |owner, cx| owner.panel.read(cx).screen.clone())
}

fn line(ready: &Ready, cx: &mut TestAppContext) -> Option<(Text, Option<ModelName>)> {
    ready
        .owner
        .read_with(cx, |owner, cx| owner.panel.read(cx).screen_line.clone())
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
fn a_hold_shows_the_window_only_while_the_stored_state_is_on(cx: &mut TestAppContext) {
    for (state, tooltip) in [
        (None, Some((Text::ScreenHoldsNotReady, None))),
        (Some(ScreenHolds::Off(ScreenOff::Unsupported)), None),
        (
            Some(ScreenHolds::Off(ScreenOff::ModelCantSee)),
            Some((Text::ScreenHoldsModelCantSee, Some(luna().display_name))),
        ),
        (
            Some(ScreenHolds::Off(ScreenOff::NotReady)),
            Some((Text::ScreenHoldsNotReady, None)),
        ),
        (
            Some(ScreenHolds::Off(ScreenOff::TurnedOff)),
            Some((Text::ScreenHoldsTurnedOff, None)),
        ),
    ] {
        let ready = screen_ready(state, cx);
        assert_eq!(icon(&ready, cx), tooltip);
        recognize(&ready.owner, 1, cx);
        assert_eq!(sent(&ready), [voice_only(1)]);
        assert_eq!(line(&ready, cx), None);
        // No lease was asked for, so nothing was covered.
        ready.owner.read_with(cx, |owner, _| {
            assert_eq!(owner.exclusion.generation(), 0);
        });
        assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    }

    let ready = screen_ready(Some(on(&luna())), cx);
    assert_eq!(
        icon(&ready, cx),
        Some((Text::ScreenHoldsOn, Some(luna().display_name)))
    );
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);
    assert_eq!(ready.fixture.borrow().affinities(), COVERED);
    assert_eq!(
        line(&ready, cx),
        Some((Text::ScreenHoldWillSee, Some(luna().display_name)))
    );
}

#[gpui::test]
fn a_refused_lease_leaves_the_hold_voice_only_and_tells_no_lapse(cx: &mut TestAppContext) {
    let ready = screen_ready(Some(on(&luna())), cx);
    ready.fixture.borrow_mut().set_failure = Some(Slot::Dock as usize);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [voice_only(1)]);
    assert_eq!(line(&ready, cx), None);
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.exclusion.generation(), 1);
        assert!(owner.exclusion.lease().is_none());
    });
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    advance(3, cx);
    assert_eq!(sent(&ready), []);
}

#[gpui::test]
fn the_target_threads_stored_model_decides_once_its_row_is_read(cx: &mut TestAppContext) {
    let shown = Some((Text::ScreenHoldsOn, Some(luna().display_name)));
    // Never read: counted as on the default model.
    let ready = screen_ready(Some(on(&luna())), cx);
    ready.owner.read_with(cx, |owner, cx| {
        assert!(owner.threads.read(cx).row(ThreadId(1)).is_none());
    });
    assert_eq!(icon(&ready, cx), shown);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);

    // Read, and on the default model: no stored choice, or the default at another effort.
    for stored in [None, Some(choice(&luna(), Effort::High))] {
        let ready = screen_ready(Some(on(&luna())), cx);
        read_row(&ready, stored, cx);
        assert_eq!(icon(&ready, cx), shown);
        recognize(&ready.owner, 1, cx);
        assert_eq!(sent(&ready), [screen_hold(1, 1)]);
    }

    // Read, and pinned to another model, one that could see the window too.
    let ready = screen_ready(Some(on(&luna())), cx);
    read_row(&ready, Some(choice(&sol(), Effort::Low)), cx);
    assert_eq!(icon(&ready, cx), Some((Text::ScreenHoldsOwnModel, None)));
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [voice_only(1)]);
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.exclusion.generation(), 0);
    });
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
}

#[gpui::test]
fn an_on_state_naming_another_model_than_the_newest_default_is_not_ready(cx: &mut TestAppContext) {
    // The default is Luna; the brain's last word still names Sol.
    let ready = screen_ready(Some(on(&sol())), cx);
    assert_eq!(icon(&ready, cx), Some((Text::ScreenHoldsNotReady, None)));
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [voice_only(1)]);
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    finish(&ready, 1, cx);

    // The brain catches up with the default, and the next hold shows Luna the window.
    receive(
        &ready.owner,
        LinkEvent::ScreenHolds {
            generation: 1,
            state: on(&luna()),
        },
        cx,
    );
    assert_eq!(
        icon(&ready, cx),
        Some((Text::ScreenHoldsOn, Some(luna().display_name)))
    );
    recognize(&ready.owner, 2, cx);
    assert_eq!(sent(&ready), [screen_hold(2, 1)]);
}

#[gpui::test]
fn the_message_names_the_lease_just_taken_and_the_recognition_pointer(cx: &mut TestAppContext) {
    let ready = screen_ready(Some(on(&luna())), cx);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);
    finish(&ready, 1, cx);
    receive(
        &ready.owner,
        LinkEvent::CaptureFailed {
            generation: 1,
            capture_for: CaptureFor::Gesture(GestureId(1)),
            reason: CaptureFailure::Cancelled,
        },
        cx,
    );
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);

    // The second hold's lease is the second generation, never the one before it.
    recognize(&ready.owner, 2, cx);
    assert_eq!(
        sent(&ready),
        [ControlToBrain::HoldRecognized {
            gesture: GestureId(2),
            thread: Some(ThreadId(1)),
            selection: 1,
            pointer: PhysicalPoint([640, -32]),
            lease: LeaseGeneration(2),
            t_qpc: 77,
        }]
    );
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.exclusion.lease().unwrap().key().generation(),
            LeaseGeneration(2)
        );
    });
}

#[gpui::test]
fn the_holds_line_follows_the_brains_outcome_and_a_disconnect_clears_the_notice(
    cx: &mut TestAppContext,
) {
    for (outcome, shown) in [
        (
            HeldScreen::NotIncluded(ScreenClass::Changed),
            Some((Text::ScreenNotIncludedChanged, None)),
        ),
        (
            HeldScreen::NotIncluded(ScreenClass::ModelCantSee),
            Some((Text::ScreenNotIncludedModel, None)),
        ),
        (HeldScreen::Attached, None),
    ] {
        let ready = screen_ready(Some(on(&luna())), cx);
        recognize(&ready.owner, 1, cx);
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
        assert_eq!(icon(&ready, cx), Some((Text::ScreenHoldsNotReady, None)));
        assert_eq!(line(&ready, cx), None);
    }
}

#[gpui::test]
fn a_lease_that_stops_protecting_a_sent_hold_tells_the_brain_once(cx: &mut TestAppContext) {
    let lapsed = [ControlToBrain::ExclusionLapsed {
        lease: LeaseGeneration(1),
    }];

    // Its timer.
    let ready = screen_ready(Some(on(&luna())), cx);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);
    advance(3, cx);
    assert_eq!(sent(&ready), lapsed);
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // A surface that can't be covered.
    let ready = screen_ready(Some(on(&luna())), cx);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);
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
    let ready = screen_ready(Some(on(&luna())), cx);
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);
    ready.fixture.borrow_mut().restore_failure = 1 << Slot::Proxy as usize;
    ready.owner.update(cx, Presence::revoke_exclusion);
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.exclusion.lease().unwrap().revoked());
    });
    assert_eq!(sent(&ready), lapsed);
    ready.fixture.borrow_mut().restore_failure = 0;
    ready.owner.update(cx, Presence::revoke_exclusion);
    ready.owner.read_with(cx, |owner, _| {
        assert!(owner.exclusion.lease().is_none());
    });
    advance(3, cx);
    assert_eq!(sent(&ready), []);
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
    recognize(&ready.owner, 1, cx);
    assert_eq!(sent(&ready), [screen_hold(1, 1)]);
    done(1, &ready, cx);
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // The first hold's lease lapses; its capture answers only after a second hold has a lease.
    let ready = screen_ready(Some(on(&luna())), cx);
    recognize(&ready.owner, 1, cx);
    advance(3, cx);
    assert_eq!(
        sent(&ready),
        [
            screen_hold(1, 1),
            ControlToBrain::ExclusionLapsed {
                lease: LeaseGeneration(1)
            }
        ]
    );
    finish(&ready, 1, cx);
    recognize(&ready.owner, 2, cx);
    assert_eq!(sent(&ready), [screen_hold(2, 2)]);
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

#[gpui::test]
fn a_lease_the_brain_never_saw_is_released_at_once_without_a_lapse(cx: &mut TestAppContext) {
    // The runs model refuses the hold after the lease is taken.
    let ready = screen_ready(Some(on(&luna())), cx);
    ready.owner.update(cx, |owner, _| {
        owner.runs.begin_hold(GestureId(1), None).unwrap();
    });
    recognize(&ready.owner, 2, cx);
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.exclusion.generation(), 1);
        assert!(owner.exclusion.lease().is_none());
        assert_eq!(owner.voice.status, Some(Text::TurnBusy));
    });
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
    // The refused hold's microphone is stopped and the hold in its way is cancelled.
    assert_eq!(
        sent(&ready),
        [
            ControlToBrain::VoiceHoldStopped {
                gesture: GestureId(2)
            },
            ControlToBrain::GestureCancel {
                gesture: GestureId(1)
            }
        ]
    );
    advance(3, cx);
    assert_eq!(sent(&ready), []);

    // The send fails after the lease is taken.
    let ready = screen_ready(Some(on(&luna())), cx);
    assert!(ready.controls.close());
    recognize(&ready.owner, 1, cx);
    ready.owner.read_with(cx, |owner, _| {
        assert_eq!(owner.exclusion.generation(), 1);
        assert!(owner.exclusion.lease().is_none());
        assert_eq!(owner.voice.status, Some(Text::HoldFailed));
    });
    assert_eq!(ready.fixture.borrow().affinities(), PRIOR);
}

#[gpui::test]
fn the_peek_draws_the_standing_icon_with_its_tooltip_and_the_holds_line(cx: &mut TestAppContext) {
    use gpui_kit::AppContext as _;
    use kumochi_protocol::message::ModelName;
    const ON: &str =
        "screen-holds on=true Hold to talk also shows GPT-6 Luna the window under your pointer";
    const OWN: &str =
        "screen-holds on=false Hold to talk is voice only in a thread with its own model";
    const LINE: &str = "peek-line GPT-6 Luna will see this window";
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
                screen: None,
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
        [ON, OWN, LINE, NOTICE].map(|selector| frame.debug_bounds(selector).is_some())
    };
    assert_eq!(drawn(cx), [false, false, false, true]);
    let luna = ModelName::new("GPT-6 Luna").unwrap();
    panel.update(cx, |panel, cx| {
        panel.screen = Some((Text::ScreenHoldsOn, Some(luna.clone())));
        panel.screen_line = Some((Text::ScreenHoldWillSee, Some(luna)));
        cx.notify();
    });
    // The hold's line takes the settings notice's place while it shows.
    assert_eq!(drawn(cx), [true, false, true, false]);
    panel.update(cx, |panel, cx| {
        panel.screen = Some((Text::ScreenHoldsOwnModel, None));
        panel.screen_line = None;
        cx.notify();
    });
    assert_eq!(drawn(cx), [false, true, false, true]);
}
