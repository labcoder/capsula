use std::{cell::RefCell, marker::PhantomData, rc::Rc};

use crate::platform::PresenceError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::presence::overlay) enum Operation {
    Configure,
    Show,
    Hide,
}

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    NotWin32,
}

#[derive(Clone, Copy)]
struct Step(Operation, Outcome);

struct Script {
    steps: [Option<Step>; 16],
    len: usize,
    used: usize,
}

thread_local! {
    static HOST: RefCell<Option<Script>> = const { RefCell::new(None) };
}

struct HostGuard(PhantomData<Rc<()>>);

impl HostGuard {
    fn install(steps: &[Step]) -> Self {
        assert!(steps.len() <= 16);
        HOST.with(|host| {
            let mut host = host.borrow_mut();
            assert!(host.is_none(), "a host script cannot replace another owner");
            let mut script = Script {
                steps: [None; 16],
                len: steps.len(),
                used: 0,
            };
            for (slot, step) in script.steps.iter_mut().zip(steps) {
                *slot = Some(*step);
            }
            *host = Some(script);
        });
        Self(PhantomData)
    }

    #[expect(
        clippy::unused_self,
        reason = "finishing consumes the guard that owns the host script"
    )]
    fn finish(self) {
        let script = HOST
            .with(|host| host.borrow_mut().take())
            .expect("the owned host script");
        assert_eq!(
            script.used, script.len,
            "every declared host result was consumed"
        );
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        HOST.with(|host| *host.borrow_mut() = None);
    }
}

pub(in crate::presence::overlay) fn native(
    operation: Operation,
) -> Option<Result<(), PresenceError>> {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        let script = host.as_mut()?;
        assert!(
            script.used < script.len,
            "the finite host script is exhausted"
        );
        let Step(expected, outcome) = script.steps[script.used].expect("a declared host result");
        assert_eq!(
            operation, expected,
            "the host operation must match the script"
        );
        script.used += 1;
        Some(match outcome {
            Outcome::Success => Ok(()),
            Outcome::NotWin32 => Err(PresenceError::NotWin32),
        })
    })
}

#[test]
fn the_host_script_is_exact_bounded_and_scoped_on_failure() {
    assert!(native(Operation::Show).is_none());
    let guard = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
        Step(Operation::Hide, Outcome::NotWin32),
    ]);
    assert!(matches!(native(Operation::Configure), Some(Ok(()))));
    assert!(matches!(native(Operation::Show), Some(Ok(()))));
    assert!(matches!(
        native(Operation::Hide),
        Some(Err(PresenceError::NotWin32))
    ));
    guard.finish();
    assert!(native(Operation::Hide).is_none());
    assert!(
        std::panic::catch_unwind(|| {
            let _guard = HostGuard::install(&[Step(Operation::Configure, Outcome::Success)]);
            native(Operation::Show)
                .expect("the script intercepts Show")
                .expect("unreachable mismatch");
        })
        .is_err()
    );
    assert!(native(Operation::Show).is_none());
    let guard = HostGuard::install(&[Step(Operation::Show, Outcome::Success); 16]);
    for _ in 0..16 {
        assert!(matches!(native(Operation::Show), Some(Ok(()))));
    }
    assert!(std::panic::catch_unwind(|| native(Operation::Show)).is_err());
    guard.finish();
    assert!(
        std::panic::catch_unwind(|| {
            HostGuard::install(&[Step(Operation::Show, Outcome::Success); 17]);
        })
        .is_err()
    );
    let guard = HostGuard::install(&[]);
    assert!(std::panic::catch_unwind(|| HostGuard::install(&[])).is_err());
    guard.finish();
}

#[derive(Clone, Copy, Debug)]
struct Draw {
    host: u64,
    painting: bool,
    returned: bool,
}

#[derive(Clone, Copy, Default)]
struct Observation {
    draws: [Option<Draw>; 16],
    len: u8,
    entered: u8,
    returned: u8,
    controls: u8,
}

thread_local! {
    static OBSERVATION: RefCell<Option<Observation>> = const { RefCell::new(None) };
}

struct Observer(PhantomData<Rc<()>>);

impl Observer {
    fn install() -> Self {
        OBSERVATION.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none());
            *slot = Some(Observation::default());
        });
        Self(PhantomData)
    }

    #[expect(
        clippy::unused_self,
        reason = "a snapshot reads the observation this guard installed"
    )]
    fn snapshot(&self) -> Observation {
        OBSERVATION.with(|slot| slot.borrow().expect("the owned observer"))
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        OBSERVATION.with(|slot| *slot.borrow_mut() = None);
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the draw hook's painting flag, as the overlay passes it"
)]
pub(in crate::presence::overlay) fn draw_enter(host: u64, painting: bool) -> Option<u8> {
    OBSERVATION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let observed = slot.as_mut()?;
        let index = observed.len;
        assert!(
            index < 16,
            "the finite DrawingElement envelope is exhausted"
        );
        observed.draws[usize::from(index)] = Some(Draw {
            host,
            painting,
            returned: false,
        });
        observed.len += 1;
        crate::allocation::checkpoint(6, 128 + index);
        Some(index)
    })
}

pub(in crate::presence::overlay) fn draw_return(index: Option<u8>) {
    let Some(index) = index else {
        return;
    };
    OBSERVATION.with(|slot| {
        let mut slot = slot.borrow_mut();
        let observed = slot
            .as_mut()
            .expect("a drawing cannot outlive its observer");
        let draw = observed.draws[usize::from(index)]
            .as_mut()
            .expect("a matching draw entry");
        assert!(!draw.returned);
        draw.returned = true;
        crate::allocation::checkpoint(6, 144 + index);
    });
}

pub(in crate::presence::overlay) fn apply_enter() {
    OBSERVATION.with(|slot| {
        if let Some(observed) = slot.borrow_mut().as_mut() {
            observed.entered += 1;
            assert_eq!(observed.entered, 1, "only one terminal owner apply");
            crate::allocation::checkpoint(6, 160);
        }
    });
}

pub(in crate::presence::overlay) fn intent(effects: &[crate::presenter::Effect]) {
    OBSERVATION.with(|slot| {
        if let Some(observed) = slot.borrow_mut().as_mut() {
            use kumochi_protocol::message::{ControlToBrain, SceneId};
            let [crate::presenter::Effect::Control(ControlToBrain::SceneFailed { scene })] =
                effects
            else {
                panic!("the sole terminal effect must be SceneFailed");
            };
            assert_eq!(*scene, SceneId(20));
            observed.controls += 1;
            assert_eq!(observed.controls, 1);
        }
    });
}

pub(in crate::presence::overlay) fn apply_return() {
    OBSERVATION.with(|slot| {
        if let Some(observed) = slot.borrow_mut().as_mut() {
            observed.returned += 1;
            assert_eq!(observed.returned, 1);
            crate::allocation::checkpoint(6, 161);
        }
    });
}

#[derive(Clone, Copy, Debug)]
enum Case {
    A,
    B,
    C,
    D,
    E,
}

#[test]
fn the_draw_observer_bounds_entries_and_requires_a_normal_return() {
    let observer = Observer::install();
    let first = draw_enter(7, true);
    assert_eq!(first, Some(0));
    let before = observer.snapshot();
    assert!(!before.draws[0].expect("the real entry").returned);
    assert_eq!((before.entered, before.returned), (0, 0));
    draw_return(first);
    for expected in 1..16 {
        let entry = draw_enter(7, false);
        assert_eq!(entry, Some(expected));
        draw_return(entry);
    }
    assert!(std::panic::catch_unwind(|| draw_enter(7, false)).is_err());
    assert_eq!(observer.snapshot().len, 16);
    assert!(std::panic::catch_unwind(Observer::install).is_err());
    drop(observer);
    assert!(OBSERVATION.with(|slot| slot.borrow().is_none()));
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the first host survives, as the episode case decides"
)]
fn prepared_geometry(
    presenter: &crate::presenter::Presenter,
    charges: &[crate::presenter::tests::HostCharges; 2],
    survivor: bool,
) {
    use super::super::super::paint_cache::{self, PaintScene, Pending};
    use kumochi_scene::{
        display::Prim,
        geom::{Affine, Vec2},
    };
    let mut index = 0;
    assert!(!presenter.frames().borrow().shown);
    assert!(!presenter.frames().borrow().paint(|frame, cache| {
        assert_eq!(
            charges[index].frame,
            frame.resident_bytes()
                + size_of::<RefCell<crate::presenter::Frames>>()
                + 2 * size_of::<usize>()
        );
        let cache = paint_cache::bound(cache, frame.generation().expect("sampled recovery frame"))
            .expect("the real staged cache");
        let mut items = frame.paint_items();
        let (item, first, prims) = items.next().expect("one recovered item");
        assert!(items.next().is_none());
        assert_eq!(first, 0);
        assert_eq!(item.transform, Affine::default());
        assert_eq!(
            (item.opacity.to_bits(), item.draw.to_bits()),
            (1.0_f32.to_bits(), 1.0_f32.to_bits())
        );
        if index == 0 {
            let [
                Prim::Rect { rect, .. },
                Prim::Text {
                    origin,
                    style,
                    text,
                    max_width,
                    ..
                },
                Prim::Line {
                    points,
                    width,
                    dashed,
                    closed,
                    ..
                },
            ] = prims
            else {
                panic!("the prepared order must be Rect/Text/Line");
            };
            assert_eq!(rect.origin, Vec2::new(0.0, 0.0));
            assert_eq!(rect.size.x.to_bits(), 28.0_f32.to_bits());
            assert_eq!(
                rect.size.y.to_bits(),
                (super::labels::HEADER_SIZE * 1.3_f32 + 14.0_f32).to_bits()
            );
            assert_eq!(*origin, Vec2::new(10.0, 7.0));
            assert_eq!(text.as_str(), "A");
            assert_eq!(style.size.to_bits(), super::labels::HEADER_SIZE.to_bits());
            assert!(style.bold);
            assert_eq!(max_width.map(f32::to_bits), Some(8.0_f32.to_bits()));
            assert_eq!(
                points.as_ref(),
                &[Vec2::new(0.0, rect.size.y), Vec2::new(28.0, rect.size.y)]
            );
            assert_eq!(width.to_bits(), 1.0_f32.to_bits());
            assert!(!dashed && !closed);
            assert_eq!(cache.pending.capacity(), 4);
            assert_eq!(
                charges[index].cache,
                size_of::<PaintScene>()
                    + size_of::<super::super::super::paint_cache::LabelEntry>()
                    + 4 * size_of::<Pending>()
            );
        } else {
            let [Prim::Rect { rect, .. }] = prims else {
                panic!("the survivor is an upright label-free rect");
            };
            assert_eq!(rect.origin, Vec2::new(120.0, 0.0));
            assert_eq!(rect.size, Vec2::new(20.0, 20.0));
            assert_eq!(cache.pending.capacity(), 2);
            assert_eq!(
                charges[index].cache,
                size_of::<PaintScene>() + 2 * size_of::<Pending>()
            );
        }
        assert_eq!(cache.pending.len(), 0);
        index += 1;
        Ok::<_, ()>(())
    }));
    assert_eq!(index, if survivor { 2 } else { 1 });
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixed owner episode keeps the five declared case boundaries together"
)]
fn episode(outer: &mut gpui_kit::TestAppContext, case: Case) {
    use super::super::super::{
        overlay_visibility::Visibility, paint_cache::Stager, paint_stroke, test_fixture,
    };
    use crate::{
        allocation,
        link::{LinkHandle, Session},
        presenter::tests::{self as fixtures, recovery_recording::Recorder},
    };
    use kumochi_protocol::message::{ControlToBrain, Incarnation, SceneId};
    use kumochi_scene::deferred;
    use std::sync::Arc;
    let (survivor, refused, pending) = match case {
        Case::A | Case::E => (false, false, 5),
        Case::B => (false, true, 9),
        Case::C => (true, false, 5),
        Case::D => (true, true, 11),
    };
    deferred::start().expect("the actual dropper already runs");
    let (mut recorder, ui, inventory) = Recorder::prepare(100);
    assert!(inventory.len() <= 64);
    let mut ui = ui
        .install()
        .unwrap_or_else(|_| panic!("vacant UI recorder"));
    recorder
        .start()
        .expect("Started precedes every affected inner owner");
    let observer = Observer::install();
    let script = match case {
        Case::C => HostGuard::install(&[
            Step(Operation::Configure, Outcome::Success),
            Step(Operation::Show, Outcome::Success),
        ]),
        Case::E => HostGuard::install(&[
            Step(Operation::Configure, Outcome::Success),
            Step(Operation::Show, Outcome::Success),
            Step(Operation::Hide, Outcome::NotWin32),
        ]),
        Case::A | Case::B | Case::D => HostGuard::install(&[
            Step(Operation::Configure, Outcome::Success),
            Step(Operation::Show, Outcome::Success),
            Step(Operation::Hide, Outcome::Success),
        ]),
    };
    let target = Arc::new(super::labels::Target::default());
    let mut inner = gpui_kit::TestAppContext::build_with_text_system(
        outer.dispatcher.clone(),
        Some("owned recovery episode"),
        Arc::new(super::labels::RefusedGlyph::Selective(Arc::clone(&target))),
    );
    let owner = test_fixture::presence(&mut inner);
    let (data, controls, charges, fixed) = owner.update(&mut inner, |this, cx| {
        this.presenter =
            fixtures::host_recovery_fixture(Box::new(Stager::new(&this.paint_scratch)), survivor);
        let (mut link, data) = LinkHandle::recording();
        let controls = link.record_controls();
        this.link = link;
        this.session = Some(Session::detached(1, Incarnation(1)));
        this.text.as_mut().expect("real typing panel").typing.open();
        this.chat.update(cx, |chat, _| {
            chat.show_for(None);
            chat.disconnected();
        });
        this.threads.update(cx, |threads, _| threads.disconnected());
        for window in [
            this.dock_window,
            this.panel_window,
            this.text.as_ref().expect("owned typing window").window,
        ] {
            window
                .update(cx, |_, window, _| {
                    assert_eq!(window.scale_factor().to_bits(), 2.0_f32.to_bits());
                })
                .expect("the other owned windows keep their declared default scale");
        }
        let ids: &[SceneId] = if survivor {
            &[SceneId(20), SceneId(21)]
        } else {
            &[SceneId(20)]
        };
        let charges = fixtures::host_ledger(&this.presenter, ids, Some(1));
        prepared_geometry(&this.presenter, &charges, survivor);
        let fixed = size_of::<Stager>()
            + 2 * size_of::<usize>()
            + size_of::<RefCell<paint_stroke::Scratch>>()
            + paint_stroke::VERTICES * size_of::<lyon::math::Point>()
            + paint_stroke::RUN_POINTS * size_of::<gpui_kit::Point<gpui_kit::Pixels>>();
        let fixed = fixed + super::super::super::paint_cache::FONT_FEATURE_BYTES;
        assert_eq!(
            crate::presenter::tests::residency(&this.presenter),
            fixed
                + charges[0].prepared()
                + charges[1].prepared()
                + charges[0].activation
                + charges[1].activation
        );
        (data, controls, charges, fixed)
    });
    if refused {
        assert!(controls.close());
    }
    let gate = recorder.hold().expect("GateEntered outside any App borrow");
    assert_eq!(
        (deferred::pending().values, deferred::pending().bytes),
        (1, 0)
    );
    let mut observation_allocation = [0; 3];
    let h1 = inner.update(|app| {
        let h1 = owner.update(app, |this, cx| {
            assert!(matches!(this.overlay, Visibility::Closed));
            this.draw_changed(cx);
            this.runs_changed(cx)
                .expect("initial real Stop/cursor update");
            let Visibility::Shown(host) = &this.overlay else {
                panic!("draw_changed must establish Shown H1");
            };
            assert!(
                host.painting.get()
                    && this.presenter.frames().borrow().shown
                    && this.activity.drawing
            );
            assert!(this.panel.read(cx).stop);
            assert!(
                this.text
                    .as_ref()
                    .expect("visible typing")
                    .view
                    .read(cx)
                    .stop
            );
            host.window
        });
        h1.update(app, |_, window, _| {
            assert_eq!(window.scale_factor().to_bits(), 2.0_f32.to_bits());
            window.set_scale_factor(1.0);
        })
        .expect("H1 scale is set while disarmed in the same outer update");
        assert_eq!(target.counts(), [0; 4]);
        assert_eq!(
            (observer.snapshot().entered, observer.snapshot().returned),
            (0, 0)
        );
        allocation::checkpoint(6, 162);
        let armed = target.arm();
        h1.update(app, |_, window, app| {
            assert_eq!(window.scale_factor().to_bits(), 1.0_f32.to_bits());
            let cleared = window.draw(app);
            allocation::checkpoint(6, 170);
            let quads = window.painted_quads();
            // Scripted Configure keeps the root's theme background; the survivor is off-window.
            assert_eq!(quads.len(), 2);
            observation_allocation = [
                quads.as_ptr().addr(),
                quads.capacity() * std::mem::size_of_val(&quads[0]),
                std::mem::align_of_val(&quads[0]),
            ];
            assert_eq!(
                (observer.snapshot().entered, observer.snapshot().returned),
                (0, 0)
            );
            assert_eq!(target.counts(), [1, 1, 1, 0]);
            drop(quads);
            allocation::checkpoint(6, 171);
            cleared.clear(app);
        })
        .expect("H1 is alive at the explicit draw/capture boundary");
        drop(armed);
        allocation::checkpoint(6, 163);
        h1
    });
    let seen = observer.snapshot();
    assert_eq!((seen.entered, seen.returned, seen.controls), (1, 1, 1));
    assert_eq!(target.counts(), [1, 1, 1, 0]);
    assert_eq!(
        target.key(),
        [
            0,
            0xFFFF_FF00,
            u64::from(super::labels::HEADER_SIZE.to_bits()),
            0,
            0,
            1_065_353_216,
            0,
            0,
            0
        ]
    );
    owner.update(&mut inner, |this, cx| {
        let ids: &[SceneId] = if matches!(case, Case::C) {
            &[SceneId(21)]
        } else {
            &[]
        };
        let now = fixtures::host_ledger(&this.presenter, ids, if refused { None } else { Some(1) });
        assert_eq!(
            crate::presenter::tests::residency(&this.presenter),
            fixed
                + if matches!(case, Case::C) {
                    charges[1].prepared() + charges[1].activation
                } else {
                    0
                }
        );
        if matches!(case, Case::C) {
            assert_eq!(now[0], charges[1]);
        }
        assert_eq!(this.activity.drawing, matches!(case, Case::C));
        assert_eq!(this.panel.read(cx).stop, matches!(case, Case::C));
        assert_eq!(
            this.text
                .as_ref()
                .expect("typing remains owned")
                .view
                .read(cx)
                .stop,
            matches!(case, Case::C)
        );
        assert!(this.thread_panel.is_none() && this.settings_panel.is_none());
        for model in [&this.chat, &this.threads] {
            let model = model.read(cx);
            assert!(
                model.rows.is_empty() && model.events.is_empty() && model.search_error.is_none()
            );
            assert!(!model.failed);
            assert!(!model.online() && !model.reading());
        }
        match case {
            Case::C => {
                let Visibility::Shown(host) = &this.overlay else {
                    panic!("the survivor must keep Shown H1");
                };
                assert_eq!(host.window, h1);
                assert!(host.painting.get());
            }
            Case::E => {
                let Visibility::EmptyHideFailed(host) = &this.overlay else {
                    panic!("hide refusal must stay terminal");
                };
                assert_eq!(host.window, h1);
                assert!(!host.painting.get());
            }
            Case::A | Case::B | Case::D => assert!(
                matches!(this.overlay, Visibility::Closed),
                "the old host closes, and no hidden host replaces it"
            ),
        }
    });
    let held = charges[0].semantic
        + charges[0].prepared()
        + charges[0].activation
        + if refused {
            if survivor {
                charges[1].semantic + charges[1].prepared() + charges[1].activation + 80
            } else {
                40
            }
        } else {
            0
        };
    assert_eq!(
        (deferred::pending().values, deferred::pending().bytes),
        (pending, held)
    );
    assert_eq!(data.len(), 0);
    assert_eq!(controls.len(), usize::from(!refused));
    drop(gate);
    if !refused {
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::SceneFailed { scene: SceneId(20) })
        );
    }
    assert_eq!(
        controls.try_recv(),
        Err(if refused {
            async_channel::TryRecvError::Closed
        } else {
            async_channel::TryRecvError::Empty
        })
    );
    if matches!(case, Case::C | Case::E) {
        allocation::checkpoint(6, 166);
        inner.update(|app| {
            owner.update(app, |this, cx| {
                this.overlay
                    .window()
                    .expect("surviving H1")
                    .drawing
                    .update(cx, |_, cx| cx.notify());
            });
            h1.update(app, |_, window, app| window.draw(app).clear(app))
                .expect("only a still-owned H1 is drawn");
        });
        allocation::checkpoint(6, 167);
    } else {
        assert!(inner.update(|app| h1.update(app, |_, _, _| ())).is_err());
    }
    owner.update(&mut inner, |this, _| {
        fixtures::host_stale_begin(&mut this.presenter, !refused);
    });
    assert_eq!(
        (observer.snapshot().entered, target.counts()),
        (1, [1, 1, 1, 0])
    );
    allocation::checkpoint(6, 168);
    drop(controls);
    drop(data);
    drop(owner);
    inner.quit();
    drop(inner);
    // The real typing panel's gpui-kit text input keeps the text system alive after teardown.
    assert_eq!(Arc::strong_count(&target), 2);
    drop(target);
    allocation::checkpoint(6, 169);
    let seen = observer.snapshot();
    drop(observer);
    script.finish();
    let dropper = recorder
        .finish()
        .expect("all producers closed before matching Finish");
    let ui = ui.finish().expect("normal UI scope return");
    assert!(!ui.lost && !ui.overflow && !dropper.lost && !dropper.overflow);
    assert_eq!((seen.entered, seen.returned, seen.controls), (1, 1, 1));
    assert!(
        seen.draws[..usize::from(seen.len)]
            .iter()
            .all(|draw| draw.is_some_and(|draw| draw.returned))
    );
    let events = &ui.requests;
    let boundary = |tag| {
        events
            .iter()
            .position(|event| (event.action, event.tag) == (6, tag))
            .expect("a declared numeric boundary")
    };
    let first = boundary(162);
    let applied = boundary(160);
    assert!(first < boundary(163) && boundary(163) < applied && applied < boundary(161));
    let handed: Vec<_> = events[first..applied]
        .iter()
        .enumerate()
        .filter(|(_, event)| event.action == 4)
        .collect();
    assert_eq!(handed.len(), 1);
    let (handoff_at, handoff) = handed[0];
    assert_eq!(
        (handoff.size, handoff.align, handoff.tag),
        (192, align_of::<gpui_kit::PathVertex<gpui_kit::Pixels>>(), 1)
    );
    let after_handoff = &events[first + handoff_at + 1..applied];
    let freed = after_handoff
        .iter()
        .position(|event| event.action == 1 && event.pointer == handoff.pointer)
        .expect("the unsubmitted line frees before actual owner apply");
    assert!(
        !after_handoff[..freed]
            .iter()
            .any(|event| event.action == 0 && event.pointer == handoff.pointer)
    );
    assert_eq!(
        (
            after_handoff[freed].size,
            after_handoff[freed].align,
            after_handoff[freed].tag
        ),
        (192, handoff.align, 1)
    );
    let requested = events[first..first + handoff_at]
        .iter()
        .rfind(|event| event.action == 0 && event.pointer == handoff.pointer)
        .expect("the pending line has covered UI creation");
    assert_eq!(
        (requested.size, requested.align, requested.tag),
        (192, handoff.align, 1)
    );
    let quad_interval = &events[boundary(170)..boundary(171)];
    assert_eq!(
        quad_interval
            .iter()
            .filter(|event| event.action == 0
                && [event.pointer, event.size, event.align] == observation_allocation)
            .count(),
        1
    );
    assert_eq!(
        quad_interval
            .iter()
            .filter(|event| event.action == 1
                && [event.pointer, event.size, event.align] == observation_allocation)
            .count(),
        1
    );
    for (index, draw) in seen.draws[..usize::from(seen.len)].iter().enumerate() {
        let draw = draw.expect("an actual root entry");
        let enter = boundary(128 + u8::try_from(index).expect("sixteen draws"));
        let returned = boundary(144 + u8::try_from(index).expect("sixteen draws"));
        assert!(enter < returned);
        if enter > boundary(163) && draw.painting {
            assert!(matches!(case, Case::C));
            assert_eq!(draw.host, h1.window_id().as_u64());
            assert_eq!(app_paths(events, enter..returned + 1), 0);
        }
    }
    if matches!(case, Case::C | Case::E) {
        let redraw = boundary(166)..boundary(167);
        assert_eq!(
            events[redraw.clone()]
                .iter()
                .filter(|event| event.action == 5)
                .count(),
            1
        );
        assert_eq!(app_paths(events, redraw.clone()), 0);
        let [allocations, bytes] = gpui(events, redraw);
        println!("recovery_redraw case={case:?} gpui_allocations={allocations} gpui_bytes={bytes}");
        assert!(
            allocations <= 91 && bytes <= 23_137,
            "GPUI's own redraw stays within its measured ceiling"
        );
    }
    let recovery = &events[first..boundary(161)];
    let [allocations, bytes] = recovery
        .iter()
        .filter(|event| event.action == 0)
        .fold([0, 0], |[count, bytes], event| {
            [count + 1, bytes + event.size]
        });
    let retained = retained(recovery);
    println!(
        "recovery_apply case={case:?} allocations={allocations} bytes={bytes} retained_allocations={} retained_bytes={}",
        retained[0], retained[1]
    );
    // Bytes include each held LayoutState's empty behind-anchor and fit maps (24 bytes each, no
    // allocation): two in the kept Scene (rest and view) and four in the recovered-scenes buffer.
    // They also include the screen unit each step's preparation carries (8 bytes, never retained).
    let bound = match case {
        // No case opens a host: a closed one is replaced only by the next show.
        Case::A => [150, 46_140, 21, 8_432],
        Case::B => [159, 46_268, 30, 8_560],
        Case::C | Case::E => [149, 46_108, 65, 19_394],
        Case::D => [162, 47_332, 33, 9_624],
    };
    assert!(
        allocations <= bound[0]
            && bytes <= bound[1]
            && retained[0] <= bound[2]
            && retained[1] <= bound[3],
        "the recovery interval stays within its measured bound"
    );
}

/// Kumochi's own outermost App scopes, where the path rule applies to App-tagged allocations.
fn app_scopes(events: &[crate::allocation::Request]) -> Vec<std::ops::Range<usize>> {
    let mut scopes = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;
    for (index, event) in events.iter().enumerate() {
        match (event.action, event.tag) {
            (2, 1) => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            (3, 1) => {
                depth = depth
                    .checked_sub(1)
                    .expect("an App scope closes after it opens");
                if depth == 0 {
                    scopes.push(start..index + 1);
                }
            }
            _ => {}
        }
    }
    assert_eq!(depth, 0, "every App scope closes");
    scopes
}

/// The path rule over Kumochi's own App scopes that overlap `range`. GPUI's own per-redraw
/// allocations are counted and reported by `gpui`; the limit applies to Kumochi's.
fn app_paths(events: &[crate::allocation::Request], range: std::ops::Range<usize>) -> usize {
    app_scopes(events)
        .into_iter()
        .filter(|scope| scope.start < range.end && range.start < scope.end)
        .map(|scope| super::super::super::paint_phase::tests::assert_paths(&events[scope], 3))
        .sum()
}

/// GPUI's allocations in `range`, as `[count, bytes]`: every one outside Kumochi's App scopes,
/// and inside them every one a GPUI, shape or fill phase tagged. The episode's ambient tag is
/// App, so a tag alone can't tell GPUI's work outside the scopes from Kumochi's.
fn gpui(events: &[crate::allocation::Request], range: std::ops::Range<usize>) -> [usize; 2] {
    let scopes = app_scopes(events);
    range
        .filter(|index| {
            events[*index].action == 0
                && (events[*index].tag != 1 || !scopes.iter().any(|scope| scope.contains(index)))
        })
        .fold([0, 0], |[count, bytes], index| {
            [count + 1, bytes + events[index].size]
        })
}

#[test]
fn the_gpui_bucket_takes_gpui_tags_inside_app_scopes_and_everything_outside() {
    let event = |action, size, tag| crate::allocation::Request {
        action,
        pointer: 0,
        size,
        align: 1,
        tag,
    };
    let events = [
        event(0, 1, 1),
        event(2, 0, 1),
        event(0, 10, 1),
        event(0, 20, 3),
        event(0, 40, 4),
        event(1, 0, 3),
        event(3, 0, 1),
        event(0, 80, 1),
    ];
    assert_eq!(gpui(&events, 0..events.len()), [4, 141]);
    assert_eq!(gpui(&events, 2..5), [2, 60]);
}

/// Allocations in `events` that no later event there frees, as `[count, bytes]`.
fn retained(events: &[crate::allocation::Request]) -> [usize; 2] {
    events
        .iter()
        .enumerate()
        .filter(|(index, event)| {
            event.action == 0
                && !events[index + 1..]
                    .iter()
                    .any(|next| next.action == 1 && next.pointer == event.pointer)
        })
        .fold([0, 0], |[count, bytes], (_, event)| {
            [count + 1, bytes + event.size]
        })
}

/// The episodes' dropper counts and recorder order are process-wide, so each runs alone in a
/// child process of this test binary.
#[test]
fn the_recovery_episodes_pass_in_fresh_processes() {
    let module = module_path!()
        .split_once("::")
        .expect("a crate-qualified module path")
        .1;
    for episode in [
        "accepting_the_last_failed_scene_closes_its_host",
        "refusing_the_last_failed_scene_disconnects_and_closes_its_host",
        "accepting_one_failed_scene_keeps_the_real_survivor_host",
        "refusing_one_failed_scene_disconnects_its_survivor",
        "a_refused_hide_leaves_the_real_host_disabled_without_retry",
    ] {
        let name = format!("{module}::{episode}");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", &name, "--ignored", "--test-threads=1"])
            .output()
            .expect("the child test process runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "{episode} passes alone in its own process"
        );
    }
}

#[gpui::test]
#[ignore = "runs in its own process from the_recovery_episodes_pass_in_fresh_processes"]
fn accepting_the_last_failed_scene_closes_its_host(cx: &mut gpui_kit::TestAppContext) {
    episode(cx, Case::A);
}

#[gpui::test]
#[ignore = "runs in its own process from the_recovery_episodes_pass_in_fresh_processes"]
fn refusing_the_last_failed_scene_disconnects_and_closes_its_host(
    cx: &mut gpui_kit::TestAppContext,
) {
    episode(cx, Case::B);
}

#[gpui::test]
#[ignore = "runs in its own process from the_recovery_episodes_pass_in_fresh_processes"]
fn accepting_one_failed_scene_keeps_the_real_survivor_host(cx: &mut gpui_kit::TestAppContext) {
    episode(cx, Case::C);
}

#[gpui::test]
#[ignore = "runs in its own process from the_recovery_episodes_pass_in_fresh_processes"]
fn refusing_one_failed_scene_disconnects_its_survivor(cx: &mut gpui_kit::TestAppContext) {
    episode(cx, Case::D);
}

#[gpui::test]
#[ignore = "runs in its own process from the_recovery_episodes_pass_in_fresh_processes"]
fn a_refused_hide_leaves_the_real_host_disabled_without_retry(cx: &mut gpui_kit::TestAppContext) {
    episode(cx, Case::E);
}

#[gpui::test]
fn first_paint_control_refusal_ends_the_real_owner_and_cancels_its_scene_wake(
    cx: &mut gpui_kit::TestAppContext,
) {
    let (owner, window, controls) = super::owned_static_canvas(cx);
    assert!(controls.close());
    let script = HostGuard::install(&[Step(Operation::Hide, Outcome::Success)]);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    script.finish();
    owner.update(&mut visual, |this, _| {
        assert!(this.session.is_none());
        assert!(!this.presenter.holds_scenes());
        assert_eq!(this.presenter.stoppable_run(), None);
        assert_eq!(this.scene_deadline, None);
        assert!(!this.paint_notice.pending.get());
        assert!(matches!(
            this.overlay,
            crate::presence::overlay_visibility::Visibility::Closed
        ));
    });
    assert_eq!(
        controls.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    );
}

#[gpui::test]
fn replacement_selects_the_successor_on_the_same_shown_host_without_an_old_ack(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneHeader, SceneId};
    use kumochi_scene::{Record, deferred::Deferred, parse_record};
    let (owner, window, controls) = super::owned_static_canvas(cx);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: SceneId(1),
            index: 0
        })
    );
    let script = HostGuard::install(&[]);
    let now = std::time::Instant::now() + std::time::Duration::from_secs(60);
    owner.update(&mut visual, |this, cx| {
        let effects = this.presenter.data(
            1,
            Deferred::new(
                DataToShell::ScenesReplaced {
                    scenes: vec![SceneId(1)],
                    successor: SceneId(2),
                },
                0,
            ),
            0,
            now,
        );
        assert!(!this.scene_effects_at(effects, now, cx));
        assert_eq!(
            this.overlay.window().expect("shown old picture").window,
            window
        );
        assert!(this.presenter.drawn());
    });
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.update(&mut visual, |this, cx| {
        let Record::Begin(begin) = parse_record(br#"{"begin":{"v":1,"scene":"next","surface":"overlay","alt":"new"}}"#).expect("header") else { panic!("begin"); };
        let effects = this.presenter.data(1, Deferred::new(DataToShell::SceneBegin {
            scene: SceneId(2), header: SceneHeader { begin, layout_version: 2 },
        }, 0), 0, now);
        assert!(!this.scene_effects_at(effects, now, cx));
        super::prepare_scene_step_at(&mut this.presenter, SceneId(2), 0,
            br#"{"step":{"id":"new","put":[{"shape":{"id":"new","kind":"rect","box":[0,0,30,30],"enter":"none"}}]}}"#, now);
        assert!(!this.scene_effects_at(Ok(Vec::new()), now, cx));
        assert_eq!(this.overlay.window().expect("shown successor").window, window);
        assert!(this.presenter.drawn());
        assert_eq!(
            crate::presenter::tests::painted(&this.presenter.frames().borrow(), SceneId(1)),
            None
        );
    });
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: SceneId(2),
            index: 0
        })
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    script.finish();
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn deleting_a_thread_resynchronizes_idle_retiring_ink_only_on_success(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::presence::overlay_visibility::Visibility;
    use kumochi_protocol::message::{
        ControlToBrain, DataToShell, Deleted, RunId, SceneId, ThreadId,
    };
    use kumochi_scene::deferred::Deferred;
    for outcome in [Deleted::Hidden, Deleted::Removed, Deleted::NotHidden] {
        let terminal = outcome != Deleted::NotHidden;
        let (owner, window, controls) = super::owned_static_canvas(cx);
        let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            controls.try_recv(),
            Ok(ControlToBrain::StepShown {
                run: RunId(6),
                scene: SceneId(1),
                index: 0
            })
        );
        let script = HostGuard::install(if terminal {
            &[Step(Operation::Hide, Outcome::Success)]
        } else {
            &[]
        });
        let now = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let expires = now + std::time::Duration::from_millis(250);
        owner.update(&mut visual, |this, cx| {
            let effects = this.presenter.data(
                1,
                Deferred::new(
                    DataToShell::ScenesReplaced {
                        scenes: vec![SceneId(1)],
                        successor: SceneId(2),
                    },
                    0,
                ),
                0,
                now,
            );
            assert!(!this.scene_effects_at(effects, now, cx));
            this.frames.idle();
            assert!(this.presenter.drawn());
            assert_eq!(this.scene_deadline, Some(expires));
            this.thread_deleted(ThreadId(99), outcome, None, cx);
            if terminal {
                // The hide closed its host, and none opens until the next show.
                assert!(matches!(&this.overlay, Visibility::Closed));
                assert!(!this.presenter.frames().borrow().shown);
                assert!(!this.presenter.drawn());
                assert_eq!(this.presenter.deadline(), None);
                assert_eq!(this.scene_deadline, None);
                assert!(this.scene_timer.is_none());
            } else {
                assert!(matches!(&this.overlay, Visibility::Shown(_)));
                assert_eq!(
                    this.overlay.window().expect("original shown host").window,
                    window
                );
                assert!(this.presenter.frames().borrow().shown);
                assert!(this.presenter.drawn());
                assert_eq!(this.presenter.deadline(), Some(expires));
                assert_eq!(this.scene_deadline, Some(expires));
            }
            assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
        });
        if terminal {
            owner.read_with(&visual, |this, _| {
                assert!(matches!(&this.overlay, Visibility::Closed));
            });
        }
        script.finish();
        close_callback_scene(&owner, cx);
    }
}

#[gpui::test]
fn a_line_s_hide_leaves_no_overlay_window_behind(cx: &mut gpui_kit::TestAppContext) {
    use crate::presence::overlay_visibility::Visibility;
    use crate::presence::screen_hold_tests::{
        detached, held, luna, on, release, screen_ready, tick,
    };
    use kumochi_protocol::geometry::PhysicalPoint;
    use kumochi_protocol::message::ControlToBrain;

    let at = PhysicalPoint([900, 320]);
    let ready = screen_ready(Some(on(&luna())), cx);
    let shown = detached(&ready, cx);
    ready.owner.update(cx, |owner, _| {
        owner.presenter.frames().borrow_mut().shown = true;
        owner.overlay = Visibility::Shown(shown);
    });
    held(&ready, 1, cx);
    tick(at, cx);
    // Only the hide is scripted: opening a window would ask the host to configure it.
    let script = HostGuard::install(&[Step(Operation::Hide, Outcome::Success)]);
    ready.owner.update(cx, |owner, _| {
        owner.counter_fixture = Some(Rc::new(|| {
            let used = HOST.with(|host| host.borrow().as_ref().map(|script| script.used));
            assert_eq!(used, Some(1), "the stamp follows the hide");
            9_001
        }));
    });
    release(&ready.owner, 1, Some(at), cx);
    cx.run_until_parked();
    script.finish();
    assert!(matches!(
        ready.controls.try_recv(),
        Ok(ControlToBrain::ScreenRelease {
            excluded_qpc: 9_001,
            ..
        })
    ));
    ready.owner.read_with(cx, |owner, _| {
        assert!(
            matches!(owner.overlay, Visibility::Closed),
            "an idle presence holds no overlay window; the next show opens one"
        );
    });
}

#[gpui::test]
fn a_cold_overlay_show_that_can_t_join_a_sent_lease_tells_the_brain_before_anything_else(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::presence::exclusion::Slot;
    use crate::presence::overlay_visibility::Visibility;
    use crate::presence::screen_hold_tests::{
        DOWN, held, luna, on, release, screen_ready, screen_release,
    };
    use kumochi_protocol::message::{ControlToBrain, LeaseGeneration};
    use kumochi_shell_core::presence::exclusion::machine::fixture::PRIOR;

    let lapsed = ControlToBrain::ExclusionLapsed {
        lease: LeaseGeneration(1),
    };
    for refused in [true, false] {
        let ready = screen_ready(Some(on(&luna())), cx);
        held(&ready, 1, cx);
        release(&ready.owner, 1, Some(DOWN), cx);
        assert_eq!(
            ready.controls.try_recv(),
            Ok(screen_release(1, 1, DOWN, &[]))
        );
        // A drawing comes due before the capture answers or the lease's timer fires.
        let fixture = ready.owner.update(cx, |owner, _| {
            assert!(matches!(owner.overlay, Visibility::Closed));
            let generation = owner
                .session
                .as_ref()
                .expect("a connected presence")
                .generation();
            let stage = super::super::super::paint_cache::Stager::new(&owner.paint_scratch);
            let (mut presenter, now, job) =
                crate::presenter::tests::preparing_for(generation, Box::new(stage));
            presenter
                .completed(job.run(&mut crate::presenter::tests::Shaper), now)
                .expect("a prepared drawing");
            let consumed = crate::presenter::tests::consume_ready(
                &mut presenter,
                crate::presenter::tests::LIVE,
                0,
                now,
                0,
                0,
            )
            .expect("the drawing becomes current");
            assert!(consumed.effects.expect("a current drawing").is_empty());
            owner.presenter = presenter;
            owner
                .exclusion_fixture
                .clone()
                .expect("the hold's surfaces")
        });
        fixture.borrow_mut().set_failure = refused.then_some(Slot::Drawing as usize);
        // A refused cover allows no native Show: the window is configured, and the drawing's
        // recovery hides it.
        let script = if refused {
            HostGuard::install(&[
                Step(Operation::Configure, Outcome::Success),
                Step(Operation::Hide, Outcome::Success),
            ])
        } else {
            HostGuard::install(&[
                Step(Operation::Configure, Outcome::Success),
                Step(Operation::Show, Outcome::Success),
            ])
        };
        ready
            .owner
            .update(cx, crate::presence::Presence::draw_changed);
        script.finish();
        let sent: Vec<_> = std::iter::from_fn(|| ready.controls.try_recv().ok()).collect();
        ready.owner.read_with(cx, |owner, _| {
            assert_eq!(owner.presenter.frames().borrow().shown, !refused);
            assert_eq!(owner.exclusion.live(LeaseGeneration(1)), !refused);
        });
        if refused {
            assert_eq!(
                sent.first(),
                Some(&lapsed),
                "the lapse precedes the drawing's recovery"
            );
            assert_eq!(sent.iter().filter(|control| **control == lapsed).count(), 1);
            assert_eq!(
                fixture.borrow().affinities(),
                PRIOR,
                "the other windows came back"
            );
        } else {
            assert_eq!(sent, []);
            fixture.borrow().assert_excluded(Slot::Drawing);
        }
    }
}

#[gpui::test]
fn an_unpainted_static_successor_sleeps_until_the_original_expiry(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneHeader, SceneId};
    use kumochi_scene::{Record, deferred::Deferred, parse_record};
    let (owner, window, controls) = super::owned_static_canvas(cx);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: SceneId(1),
            index: 0
        })
    );
    let script = HostGuard::install(&[]);
    let now = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let expires = now + std::time::Duration::from_millis(250);
    owner.update(&mut visual, |this, cx| {
        let effects = this.presenter.data(1, Deferred::new(DataToShell::ScenesReplaced {
            scenes: vec![SceneId(1)], successor: SceneId(2),
        }, 0), 0, now);
        assert!(!this.scene_effects_at(effects, now, cx));
        let Record::Begin(begin) = parse_record(br#"{"begin":{"v":1,"scene":"next","surface":"overlay","alt":"new"}}"#).expect("header") else { panic!("begin"); };
        let effects = this.presenter.data(1, Deferred::new(DataToShell::SceneBegin {
            scene: SceneId(2), header: SceneHeader { begin, layout_version: 2 },
        }, 0), 0, now);
        assert!(!this.scene_effects_at(effects, now, cx));
        super::prepare_scene_step_at(&mut this.presenter, SceneId(2), 0,
            br#"{"step":{"id":"first","put":[{"shape":{"id":"first","kind":"rect","box":[0,0,30,30],"enter":"none"}}]}}"#, now);
        super::prepare_scene_step_at(&mut this.presenter, SceneId(2), 1,
            br#"{"step":{"id":"next","start":"now","put":[{"shape":{"id":"next","kind":"rect","box":[40,0,30,30],"enter":"none"}}]}}"#, now);
        assert!(!this.scene_effects_at(Ok(Vec::new()), now, cx));
        assert_eq!(
            crate::presenter::tests::painted(&this.presenter.frames().borrow(), SceneId(2)),
            None
        );
        assert_eq!(this.presenter.next_wake(now), None);
        assert_eq!(this.presenter.deadline(), Some(expires));
        assert_eq!(this.scene_deadline, Some(expires));
        assert!(this.scene_timer.is_some());
        assert!(!this.draw_changed_at(now + std::time::Duration::from_millis(249), cx));
        assert_eq!(this.scene_deadline, Some(expires));
        assert_eq!(this.presenter.take_shown(1), [None; 4]);
        assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    });
    script.finish();
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn escape_before_a_successor_run_is_known_discards_ink_without_fabricating_a_stop(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneId};
    use kumochi_scene::deferred::Deferred;
    let (owner, window, controls) = super::owned_static_canvas(cx);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: SceneId(1),
            index: 0,
        })
    );
    let script = HostGuard::install(&[Step(Operation::Hide, Outcome::Success)]);
    owner.update(&mut visual, |this, cx| {
        let now = std::time::Instant::now();
        let effects = this.presenter.data(
            1,
            Deferred::new(
                DataToShell::ScenesReplaced {
                    scenes: vec![SceneId(1)],
                    successor: SceneId(2),
                },
                0,
            ),
            0,
            now,
        );
        assert!(!this.scene_effects_at(effects, now, cx));
        assert!(this.presenter.drawn());
        assert_eq!(this.presenter.stoppable_run(), None);
        this.activity = crate::presence::Presence::activity(
            &this.runs,
            crate::escape::Panel::Closed,
            &this.presenter,
        );
        this.escape(cx)
            .expect("Esc disposes the unbound visual hold");
        assert!(!this.presenter.drawn());
        assert_eq!(this.scene_deadline, None);
        assert_eq!(this.presenter.deadline(), None);
    });
    script.finish();
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn a_refused_host_show_reports_a_scene_failure_instead_of_a_user_clear(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::{ControlToBrain, SceneId};
    let now = std::time::Instant::now();
    let (owner, controls) = callback_scene(
        br#"{"step":{"id":"static","put":[{"shape":{"id":"box","kind":"rect","box":[0,0,30,30],"enter":"none"}}]}}"#,
        now, cx,
    );
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::NotWin32),
        Step(Operation::Hide, Outcome::Success),
    ]);
    owner.update(cx, |this, cx| {
        assert!(!this.draw_changed_at(now, cx));
        assert!(!this.presenter.holds_scenes());
        assert_eq!(this.scene_deadline, None);
    });
    script.finish();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::SceneFailed { scene: SceneId(1) })
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn a_failure_after_success_is_drained_before_any_paint_acknowledgement(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::{ControlToBrain, SceneId};
    let (owner, window, controls) = super::owned_static_canvas(cx);
    let script = HostGuard::install(&[Step(Operation::Hide, Outcome::Success)]);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        owner.update(cx, |this, _| {
            assert!(this.paint_notice.pending.get());
            assert!(
                this.presenter
                    .frames()
                    .borrow()
                    .paint(|_, _| Err::<(), ()>(()))
            );
        });
    });
    script.finish();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::SceneFailed { scene: SceneId(1) })
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.update(&mut visual, |this, _| {
        assert!(!this.presenter.holds_scenes());
        assert_eq!(
            this.session.as_ref().map(crate::link::Session::generation),
            Some(1)
        );
        assert!(!this.paint_notice.pending.get());
        assert!(matches!(
            this.overlay,
            crate::presence::overlay_visibility::Visibility::Closed
        ));
    });
}

#[gpui::test]
fn the_real_busy_notification_reaches_consumption_and_the_first_canvas_paint(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::{link::Session, presenter::Effect};
    use kumochi_protocol::message::{ControlToBrain, DataToShell, Incarnation, RunId, SceneId};
    use kumochi_scene::{Record, deferred::Deferred, parse_record};
    use std::time::Instant;
    let mut presenter = super::ready_canvas();
    let Record::Step(step) =
        parse_record(br#"{"step":{"id":"tail","start":"now"}}"#).expect("tail")
    else {
        panic!("step");
    };
    let now = Instant::now();
    let effects = presenter
        .data(
            1,
            Deferred::new(
                DataToShell::SceneStepCommit {
                    run: RunId(6),
                    scene: SceneId(1),
                    index: 1,
                    step,
                },
                100,
            ),
            100,
            now,
        )
        .expect("real admission lane");
    let mut jobs = effects.into_iter().filter_map(|effect| match effect {
        Effect::Admit(job) => Some(job),
        _ => None,
    });
    let job = jobs.next().expect("one admission");
    assert!(jobs.next().is_none());
    assert!(presenter.advance(1, now).expect("Busy").is_empty());
    assert_eq!(presenter.next_wake(now), None);
    let effects: Vec<_> = presenter
        .completed(job.run(), now)
        .expect("admission returned")
        .into_iter()
        .filter(|effect| matches!(effect, Effect::ConsumptionReady { .. }))
        .collect();
    assert_eq!(effects.len(), 1);
    let owner = crate::presence::test_fixture::presence(cx);
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    let (window, controls) = owner.update(cx, |this, cx| {
        this.presenter = presenter;
        this.session = Some(Session::detached(1, Incarnation(1)));
        let controls = this.link.record_controls();
        this.scene_effects(Ok(effects), cx);
        (
            this.overlay.window().expect("activated real host").window,
            controls,
        )
    });
    script.finish();
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: SceneId(1),
            index: 0
        })
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    owner.update(&mut visual, |this, cx| {
        this.presenter.disconnected();
        this.scene_timer = None;
        this.scene_deadline = None;
        this.overlay
            .window()
            .expect("fixture host")
            .window
            .update(cx, |_, window, _| window.remove_window())
            .expect("close fixture host");
        this.overlay = crate::presence::overlay_visibility::Visibility::Closed;
    });
}

#[gpui::test]
fn provider_done_with_a_shown_rest_and_a_future_step_stops_then_clears(
    cx: &mut gpui_kit::TestAppContext,
) {
    use kumochi_protocol::message::{ControlToBrain, DataToShell, Phase, RunId, SceneId, ThreadId};
    use std::time::{Duration, Instant};
    let (owner, window, controls) = super::owned_static_canvas(cx);
    let mut visual = gpui_kit::VisualTestContext::from_window(window, cx);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: SceneId(1),
            index: 0,
        })
    );
    let script = HostGuard::install(&[Step(Operation::Hide, Outcome::Success)]);
    owner.update(&mut visual, |this, cx| {
        super::prepare_canvas_step(&mut this.presenter, 1,
            br#"{"step":{"id":"future","start":{"after":"static","ms":2200},"put":[{"shape":{"id":"later","kind":"rect","box":[30,0,20,20],"enter":"none"}}]}}"#);
        this.draw_changed(cx);
        assert!(this.scene_deadline.is_some());
        this.runs.connected(1);
        this.runs.apply_state(RunId(6), ThreadId(1), Phase::Answering, None);
        this.runs.apply_state(RunId(6), ThreadId(1), Phase::Done, None);
        this.runs.apply_data(DataToShell::RunEnd { run: RunId(6) }, 24);
        assert_eq!(this.runs.stop(), None);
        this.activity = crate::presence::Presence::activity(
            &this.runs, crate::escape::Panel::Closed, &this.presenter,
        );
        assert!(this.activity.working);
        this.escape(cx).expect("first Esc stops pending drawing work");
        assert_eq!(this.scene_deadline, None);
        assert_eq!(this.presenter.next_wake(Instant::now()), None);
        let after_due = Instant::now() + Duration::from_secs(4);
        assert!(this.presenter.advance(1, after_due).expect("halted future").is_empty());
        assert!(!this.presenter.sample_at(after_due).expect("a sample").0.changed);
        assert_eq!(this.presenter.take_shown(1), [None; 4]);
        this.text.as_mut().expect("typing panel").typing.open();
        this.escape(cx).expect("second Esc clears before cut acknowledgement");
        assert!(!this.presenter.holds_scenes());
        // The hide closed its host, and none opens until the next show.
        assert!(matches!(this.overlay, crate::presence::overlay_visibility::Visibility::Closed));
    });
    owner.read_with(&visual, |this, _| {
        assert!(matches!(
            this.overlay,
            crate::presence::overlay_visibility::Visibility::Closed
        ));
    });
    script.finish();
    let ControlToBrain::StopScenes { run, cuts } = controls.try_recv().expect("StopScenes") else {
        panic!("covering Stop");
    };
    assert_eq!(run, RunId(6));
    assert_eq!(cuts.len(), 1);
    assert_eq!(cuts[0].scene, SceneId(1));
    assert_eq!(cuts[0].cut.activated, 1);
    assert_eq!(
        &*cuts[0].cut.revealed,
        &[kumochi_scene::Handle::new(1, 0).expect("literal first object")]
    );
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::ClearDrawing {
            scene_ids: vec![SceneId(1)]
        })
    );
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
}

fn callback_scene(
    record: &[u8],
    origin: std::time::Instant,
    cx: &mut gpui_kit::TestAppContext,
) -> (
    gpui_kit::Entity<crate::presence::Presence>,
    async_channel::Receiver<kumochi_protocol::message::ControlToBrain>,
) {
    use crate::{
        link::Session,
        presence::{paint_cache::Stager, test_fixture},
    };
    use kumochi_protocol::message::Incarnation;
    let mut presenter = super::begun_at(Box::new(Stager(size_of::<Stager>())), origin);
    super::prepare_canvas_step_at(&mut presenter, 0, record, origin);
    assert!(
        presenter
            .advance(1, origin)
            .expect("activate fixture")
            .is_empty()
    );
    let owner = test_fixture::presence(cx);
    let controls = owner.update(cx, |this, _| {
        this.presenter = presenter;
        this.session = Some(Session::detached(1, Incarnation(1)));
        assert!(this.frames.want(origin));
        this.link.record_controls()
    });
    (owner, controls)
}

fn retained_x(
    owner: &gpui_kit::Entity<crate::presence::Presence>,
    cx: &gpui_kit::TestAppContext,
) -> Vec<f32> {
    owner.read_with(cx, |this, _| {
        let mut x = Vec::new();
        assert!(!this.presenter.frames().borrow().paint(|frame, _| {
            x.extend(frame.items().iter().map(|item| item.transform.0[4]));
            Ok::<_, ()>(())
        }));
        x
    })
}

fn close_callback_scene(
    owner: &gpui_kit::Entity<crate::presence::Presence>,
    cx: &mut gpui_kit::TestAppContext,
) {
    owner.update(cx, |this, cx| {
        this.end_scene_connection();
        if let Some(host) = this.overlay.window() {
            host.window
                .update(cx, |_, window, _| window.remove_window())
                .expect("close fixture overlay");
        }
        this.overlay = crate::presence::overlay_visibility::Visibility::Closed;
    });
}

fn retained_draw(
    owner: &gpui_kit::Entity<crate::presence::Presence>,
    cx: &gpui_kit::TestAppContext,
) -> Vec<f32> {
    owner.read_with(cx, |this, _| {
        let mut draw = Vec::new();
        assert!(!this.presenter.frames().borrow().paint(|frame, _| {
            draw.extend(frame.items().iter().map(|item| item.draw));
            Ok::<_, ()>(())
        }));
        draw
    })
}

/// One vsync on each window the presence shows. Windows sends a hidden window no `WM_PAINT`,
/// so a frame callback armed on one waits until it's shown again. Returns the callbacks run.
fn vsync_shown(
    owner: &gpui_kit::Entity<crate::presence::Presence>,
    cx: &mut gpui_kit::TestAppContext,
) -> usize {
    use crate::presence::geometry::PanelVisibility;
    let shown = owner.read_with(cx, |this, _| {
        [
            Some(this.dock_window),
            (this.panel_visibility == PanelVisibility::Visible).then_some(this.panel_window),
            this.overlay.window().map(|host| host.window),
        ]
    });
    shown
        .into_iter()
        .flatten()
        .map(|window| {
            window
                .update(cx, |_, window, cx| window.simulate_next_frame(cx))
                .expect("a shown fixture window")
        })
        .sum()
}

/// The recorded run-8 call: a two-`Q` parabola drawn in over 800 ms, with axes and labels,
/// asked for from the typing panel, which keeps the status panel hidden.
#[gpui::test]
fn the_recorded_graph_finishes_drawing_while_the_status_panel_is_hidden(
    cx: &mut gpui_kit::TestAppContext,
) {
    use std::time::{Duration, Instant};
    let origin = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .expect("the monotonic clock is past its second second");
    let (owner, _controls) = callback_scene(
        r#"{"step":{"id":"axes_and_curve","put":[{"line":{"id":"x_axis","from":[40,260],"to":[440,260],"color":"muted","width":2}},{"line":{"id":"y_axis","from":[240,410],"to":[240,40],"color":"muted","width":2}},{"path":{"id":"parabola","d":"M 100 60 Q 170 230 240 260 Q 310 230 380 60","color":"blue","width":4,"enter":"draw","dur":800}},{"text":{"id":"graph_label","at":[240,20],"text":"y = x²","size":20,"bold":true,"align":"center","color":"fg"}},{"text":{"id":"x_label","at":[445,265],"text":"x","size":15,"color":"fg"}},{"text":{"id":"y_label","at":[246,40],"text":"y","size":15,"color":"fg"}},{"text":{"id":"origin_label","at":[224,280],"text":"0","size":13,"color":"muted"}}]}}"#.as_bytes(),
        origin,
        cx,
    );
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    owner.update(cx, |this, cx| {
        this.text.as_mut().expect("typing owner").typing.open();
        this.frames.idle();
        assert!(this.draw_changed_at(origin + Duration::from_millis(255), cx));
        assert!(
            !this
                .presenter
                .frames()
                .borrow()
                .paint(|_, _| Ok::<_, ()>(()))
        );
        assert!(
            this.presenter
                .begin_painted(origin + Duration::from_millis(255)),
            "the recorded first frame paints at 255 ms"
        );
        this.want_frames(cx);
    });
    script.finish();
    assert_eq!(
        retained_draw(&owner, cx),
        [0.809_890_6],
        "the recorded first frame: the x axis draws in and the rest wait their turn"
    );
    assert_eq!(vsync_shown(&owner, cx), 1);
    assert_eq!(retained_draw(&owner, cx), [1.0; 7]);
    assert_eq!(vsync_shown(&owner, cx), 0);
    close_callback_scene(&owner, cx);
}

/// A first paint that lands after the step's activation clock has run out: dock frames keep its
/// first sample, and the curve draws its authored 800 ms from the first frame after that paint.
#[gpui::test]
fn a_late_first_paint_starts_the_entrance_from_its_initial_pose(cx: &mut gpui_kit::TestAppContext) {
    use kumochi_protocol::message::{ControlToBrain, RunId};
    use std::time::{Duration, Instant};
    let origin = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .expect("the monotonic clock is past its second second");
    let at = |ms| origin + Duration::from_millis(ms);
    let (owner, controls) = callback_scene(
        br#"{"step":{"id":"curve","put":[{"path":{"id":"parabola","d":"M 100 60 Q 170 230 240 260 Q 310 230 380 60","color":"blue","width":4,"enter":"draw","dur":800}}]}}"#,
        origin,
        cx,
    );
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    let before = Instant::now();
    owner.update(cx, |this, cx| {
        this.frames.idle();
        assert!(this.draw_changed_at(at(0), cx));
        assert!(this.draw_changed_at(at(300), cx));
        assert!(
            !this.draw_changed_at(at(900), cx),
            "the activation's own clock has run out"
        );
    });
    let after = Instant::now();
    script.finish();
    assert_eq!(
        controls.try_recv(),
        Ok(ControlToBrain::StepShown {
            run: RunId(6),
            scene: super::TWINNED,
            index: 0,
        })
    );
    assert_eq!(
        retained_draw(&owner, cx),
        [0.0],
        "the first paint is the initial pose"
    );
    let end = owner.update(cx, |this, _| {
        assert!(
            !this.frames.want(Instant::now()),
            "the paint woke the drawing's frames"
        );
        this.presenter.next_wake(after).expect("the entrance's end")
    });
    assert!(
        before + Duration::from_millis(799) < end && end <= after + Duration::from_millis(800),
        "the entrance ends 800 ms after its observed first paint"
    );
    let start = end
        .checked_sub(Duration::from_millis(800))
        .expect("the entrance starts after the monotonic clock's origin");
    for (late, curve) in [(400, 0.875), (600, 0.984_375), (800, 1.0)] {
        owner.update(cx, |this, cx| {
            this.draw_changed_at(start + Duration::from_millis(late), cx);
        });
        assert_eq!(retained_draw(&owner, cx), [curve], "{late} ms into it");
    }
    close_callback_scene(&owner, cx);
    assert_eq!(vsync_shown(&owner, cx), 1, "the frame the paint armed");
    assert_eq!(vsync_shown(&owner, cx), 0);
}

/// A step whose only object appears 200 ms in, reached by a wake that runs at 300 ms: the kept
/// first sample is the object's initial pose at 200 ms, and its 800 ms entrance runs from the
/// paint observed at 400 ms.
#[gpui::test]
fn a_late_appearance_wake_keeps_the_delayed_objects_initial_pose(
    cx: &mut gpui_kit::TestAppContext,
) {
    use std::time::{Duration, Instant};
    let origin = Instant::now()
        .checked_sub(Duration::from_secs(2))
        .expect("the monotonic clock is past its second second");
    let at = |ms| origin + Duration::from_millis(ms);
    let (owner, _controls) = callback_scene(
        br#"{"step":{"id":"curve","put":[{"path":{"id":"parabola","d":"M 100 60 Q 170 230 240 260 Q 310 230 380 60","color":"blue","width":4,"enter":"draw","dur":800,"delay":200}}]}}"#,
        origin,
        cx,
    );
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    owner.update(cx, |this, cx| {
        this.frames.idle();
        assert!(!this.draw_changed_at(at(0), cx), "nothing has appeared");
        assert!(
            this.draw_changed_at(at(300), cx),
            "the late appearance wake"
        );
        assert!(
            !this
                .presenter
                .frames()
                .borrow()
                .paint(|_, _| Ok::<_, ()>(()))
        );
        assert!(this.presenter.begin_painted(at(400)));
    });
    script.finish();
    assert_eq!(
        retained_draw(&owner, cx),
        [0.0],
        "the first paint is the initial pose"
    );
    for (late, curve) in [(800, 0.875), (1200, 1.0)] {
        owner.update(cx, |this, cx| {
            this.draw_changed_at(at(late), cx);
        });
        assert_eq!(retained_draw(&owner, cx), [curve], "{late} ms");
    }
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn the_last_kumo_callback_keeps_an_already_due_delayed_rectangle_wake(
    cx: &mut gpui_kit::TestAppContext,
) {
    use std::time::{Duration, Instant};
    let origin = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("the monotonic clock is past its first second");
    let (owner, _controls) = callback_scene(br#"{"step":{"id":"delayed","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none","delay":200}}]}}"#, origin, cx);
    let panel = owner.update(cx, |this, _| {
        this.text.as_mut().expect("typing owner").typing.open();
        this.panel_window
    });
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    panel
        .update(cx, |_, window, cx| {
            owner.update(cx, |this, cx| {
                this.frame_at(origin + Duration::from_millis(199), window, cx)
                    .expect("last Kumo callback");
                assert_eq!(
                    this.scene_deadline,
                    Some(origin + Duration::from_millis(200))
                );
                assert!(!this.presenter.drawn());
            });
            assert_eq!(window.simulate_next_frame(cx), 0);
        })
        .expect("held panel window");
    assert_eq!(retained_x(&owner, cx), Vec::<f32>::new());
    cx.run_until_parked();
    script.finish();
    assert_eq!(retained_x(&owner, cx), [0.0]);
    owner.update(cx, |this, _| {
        assert_eq!(this.scene_deadline, None);
        assert!(this.scene_timer.is_none());
        assert!(this.frames.want(Instant::now()));
        this.frames.idle();
    });
    panel
        .update(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 0);
        })
        .expect("resting panel");
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn a_penultimate_pose_keeps_one_callback_to_sample_the_final_pose_then_rests(
    cx: &mut gpui_kit::TestAppContext,
) {
    use std::time::{Duration, Instant};
    let origin = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("the monotonic clock is past its first second");
    let (owner, _controls) = callback_scene(br#"{"step":{"id":"motion","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none"}},{"anim":{"target":"r","track":"x","ease":"linear","keys":[[0,0],[200,200]]}}]}}"#, origin, cx);
    let panel = owner.update(cx, |this, _| {
        this.text.as_mut().expect("typing owner").typing.open();
        this.panel_window
    });
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    panel
        .update(cx, |_, window, cx| {
            owner.update(cx, |this, cx| {
                this.frame_at(origin + Duration::from_millis(199), window, cx)
                    .expect("penultimate sample");
                assert!(
                    !this
                        .presenter
                        .frames()
                        .borrow()
                        .paint(|_, _| Ok::<_, ()>(()))
                );
                assert!(
                    this.presenter
                        .begin_painted(origin + Duration::from_millis(199))
                );
            });
        })
        .expect("held panel");
    script.finish();
    assert_eq!(retained_x(&owner, cx), [199.0]);
    panel
        .update(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 1);
        })
        .expect("final callback");
    assert_eq!(retained_x(&owner, cx), [200.0]);
    owner.update(cx, |this, _| {
        assert_eq!(this.scene_deadline, None);
        assert!(this.scene_timer.is_none());
        assert!(this.frames.want(Instant::now()));
        this.frames.idle();
    });
    panel
        .update(cx, |_, window, cx| {
            assert_eq!(window.simulate_next_frame(cx), 0);
        })
        .expect("no periodic callbacks");
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn first_visibility_places_refused_escape_stop_in_the_status_panel_from_a_dock_frame(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::presence::geometry::PanelVisibility;
    use std::time::{Duration, Instant};
    let origin = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("the monotonic clock is past its first second");
    let (owner, _controls) = callback_scene(br#"{"step":{"id":"first","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none","delay":200}},{"anim":{"target":"r","track":"x","keys":[[0,0],[1000,100]]}}]}}"#, origin, cx);
    let dock = owner.read_with(cx, |this, _| this.dock_window);
    dock.update(cx, |_, window, cx| {
        owner.update(cx, |this, cx| {
            assert_eq!(this.panel_visibility, PanelVisibility::Hidden);
            this.frame_at(origin + Duration::from_millis(199), window, cx)
                .expect("invisible moving sample");
            assert!(!this.activity.drawing);
            assert!(!this.panel.read(cx).stop);
        });
    })
    .expect("held dock");
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
    ]);
    dock.update(cx, |_, window, cx| {
        assert_eq!(window.simulate_next_frame(cx), 1);
    })
    .expect("real first-visible callback");
    script.finish();
    owner.read_with(cx, |this, cx| {
        assert!(this.escape_hold.shows_stop());
        assert!(this.panel.read(cx).stop);
        assert!(this.activity.drawing);
        assert_eq!(this.panel_visibility, PanelVisibility::Visible);
    });
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn a_failed_stop_placement_ends_the_connection_without_a_manual_retry(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::presence::geometry::PanelVisibility;
    use std::time::Instant;
    let (owner, _overlay, _controls) = super::owned_static_canvas(cx);
    let panel = owner.read_with(cx, |this, _| this.panel_window);
    let removed = owner.read_with(cx, |this, _| this.dock_window);
    removed
        .update(cx, |_, window, _| window.remove_window())
        .expect("remove fixture placement handle");
    let script = HostGuard::install(&[Step(Operation::Hide, Outcome::Success)]);
    owner.update(cx, |this, cx| {
        this.panel_window = removed;
        assert!(this.runs_changed(cx).is_err());
        assert!(this.session.is_none());
        assert!(!this.presenter.holds_scenes());
        assert!(!this.activity.working);
        assert!(!this.activity.drawing);
        assert_eq!(this.panel_visibility, PanelVisibility::Hidden);
        assert!(!this.escape_hold.shows_stop());
        assert!(!this.panel.read(cx).stop);
        this.panel_window = panel;
    });
    script.finish();
    panel
        .update(cx, |_, window, cx| {
            owner.update(cx, |this, cx| {
                this.frame_at(Instant::now(), window, cx)
                    .expect("terminal connection stays empty");
                assert!(!this.activity.drawing);
                assert_eq!(this.panel_visibility, PanelVisibility::Hidden);
                assert_eq!(this.scene_deadline, None);
                assert!(this.scene_timer.is_none());
            });
            assert_eq!(window.simulate_next_frame(cx), 0);
        })
        .expect("restored handle cannot restore failed drawing");
    close_callback_scene(&owner, cx);
}

#[gpui::test]
fn persistent_stop_configuration_refusal_retires_motion_with_an_actionable_notice(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::{chat_notice::Source, presence::geometry::PanelVisibility};
    use kumochi_protocol::message::{Action, ErrorArg, ErrorKey, GestureId, UserError};
    use std::time::{Duration, Instant};

    let origin = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("the monotonic clock is past its first second");
    let (owner, controls) = callback_scene(br#"{"step":{"id":"motion","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none","delay":200}},{"anim":{"target":"r","track":"x","ease":"linear","keys":[[0,0],[1000,100]]}}]}}"#, origin, cx);
    let dock = owner.update(cx, |this, cx| {
        this.chat_notice.present(
            UserError::new(
                ErrorKey::SignInNeeded,
                vec![ErrorArg::Count(2)],
                vec![Action::SignIn],
            )
            .expect("nonempty actionable notice"),
            Source::Account,
        );
        this.runs_changed(cx).expect("semantic conversation update");
        assert!(
            !crate::presenter::tests::select(&mut this.presenter.input, GestureId(41)).is_empty()
        );
        assert!(matches!(
            this.presenter.input.configure(&mut this.input_windows),
            Err(PresenceError::SelectionSurface)
        ));
        this.dock_window
    });
    let script = HostGuard::install(&[
        Step(Operation::Configure, Outcome::Success),
        Step(Operation::Show, Outcome::Success),
        Step(Operation::Hide, Outcome::Success),
    ]);
    dock.update(cx, |_, window, cx| {
        owner.update(cx, |this, cx| {
            assert!(matches!(
                this.frame_at(origin + Duration::from_millis(220), window, cx),
                Err(crate::presence::OpenError::Native(
                    PresenceError::SelectionSurface
                ))
            ));
            assert!(this.session.is_none());
            assert!(!this.presenter.holds_scenes());
            assert_eq!(this.presenter.stoppable_run(), None);
            assert!(!this.activity.working);
            assert!(!this.activity.drawing);
            assert_eq!(this.panel_visibility, PanelVisibility::Hidden);
            assert!(!this.escape_hold.shows_stop());
            assert!(!this.panel.read(cx).stop);
            // The hide closed its host, and none opens until the next show.
            assert!(matches!(
                this.overlay,
                crate::presence::overlay_visibility::Visibility::Closed
            ));
            assert_eq!(this.scene_deadline, None);
            assert!(this.scene_timer.is_none());
        });
        assert_eq!(window.simulate_next_frame(cx), 0);
    })
    .expect("held dock");
    owner.read_with(cx, |this, _| {
        assert!(matches!(
            this.overlay,
            crate::presence::overlay_visibility::Visibility::Closed
        ));
    });
    script.finish();
    assert_eq!(retained_x(&owner, cx), Vec::<f32>::new());
    for at in [240, 260, 280] {
        dock.update(cx, |_, window, cx| {
            owner.update(cx, |this, cx| {
                assert!(matches!(
                    this.presenter.input.configure(&mut this.input_windows),
                    Err(PresenceError::SelectionSurface)
                ));
                this.frame_at(origin + Duration::from_millis(at), window, cx)
                    .expect("later frame cannot retry the failed operation");
                let notice = this.chat_notice.shown().expect("notice remains retained");
                assert_eq!(notice.error.args(), &[ErrorArg::Count(2)]);
                assert_eq!(notice.error.actions(), &[Action::SignIn]);
                assert!(this.session.is_none());
                assert!(!this.presenter.holds_scenes());
                assert_eq!(this.scene_deadline, None);
                assert!(this.scene_timer.is_none());
            });
            assert_eq!(window.simulate_next_frame(cx), 0);
        })
        .expect("subsequent dock frame");
    }
    owner.update(cx, |this, cx| {
        assert!(this.frames.want(Instant::now()));
        this.frames.idle();
        assert_eq!(this.presenter.input.selection(), Some(GestureId(41)));
        this.escape(cx)
            .expect("existing selection cancellation route");
        assert_eq!(this.presenter.input.selection(), None);
    });
    assert_eq!(controls.try_recv(), Err(async_channel::TryRecvError::Empty));
    close_callback_scene(&owner, cx);
}
