use std::collections::BTreeMap;

use gpui_kit::{MouseButton, point, px};
use kumochi_protocol::message::GestureId;

use super::{
    Effect, ExitCause, InputPlatform, Layer, OverlayInput, OverlayOwner, RegionId, Regions,
    SelectionEnd,
};
use crate::presence::drag::{DragOwner, End, Event, Outcome};

const A: RegionId = RegionId(1);
const B: RegionId = RegionId(2);
const HOLD: GestureId = GestureId(1);
const RECT: [i32; 4] = [-20, 30, 100, 80];
const REST: [Layer; 5] = [
    Layer::Panels,
    Layer::Dock,
    Layer::Regions,
    Layer::Overlay,
    Layer::Selection,
];
const PICK: [Layer; 5] = [
    Layer::Selection,
    Layer::Panels,
    Layer::Dock,
    Layer::Regions,
    Layer::Overlay,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeStep {
    DisableRegions,
    Raise,
    Capture,
}

#[derive(Default)]
struct Platform {
    mode: OverlayInput,
    regions: Regions,
    orders: Vec<[Layer; 5]>,
    orbit: Option<RegionId>,
    drag: DragOwner,
    preempted: Vec<End>,
    starts: Vec<GestureId>,
    endings: Vec<(GestureId, ExitCause, SelectionEnd)>,
    fail: Option<NativeStep>,
    selection_above: bool,
    captured: bool,
    input: crate::platform::input::Input,
    last_loss: Option<(u64, Option<MouseButton>)>,
}

impl InputPlatform for Platform {
    type Error = NativeStep;

    fn configure(&mut self, mode: OverlayInput, regions: &Regions) -> Result<(), NativeStep> {
        self.mode = mode;
        self.regions = if mode == OverlayInput::Regions {
            regions.clone()
        } else {
            BTreeMap::new()
        };
        if mode == OverlayInput::Selecting {
            if self.drag.live().is_some() || self.orbit.is_some() {
                return Err(NativeStep::Capture);
            }
            for step in [
                NativeStep::DisableRegions,
                NativeStep::Raise,
                NativeStep::Capture,
            ] {
                if self.fail == Some(step) {
                    return Err(step);
                }
                match step {
                    NativeStep::DisableRegions => self.regions.clear(),
                    NativeStep::Raise => self.selection_above = true,
                    NativeStep::Capture => {
                        self.last_loss = Some(self.input.capture_changed());
                        self.captured = true;
                    }
                }
            }
        } else {
            self.selection_above = false;
            self.captured = false;
        }
        self.orders.push(mode.order());
        Ok(())
    }
}

impl Platform {
    #[expect(
        clippy::panic_in_result_fn,
        reason = "only native faults return Err; broken test invariants must fail"
    )]
    fn apply(&mut self, owner: &OverlayOwner, effects: Vec<Effect>) -> Result<(), NativeStep> {
        for effect in effects {
            match effect {
                Effect::InputCancelled(id) => {
                    assert_eq!(self.orbit.take(), Some(id));
                }
                Effect::CancelDrag(_) => {
                    let Outcome::Ended(_, end) = self.drag.handle(Event::Preempt) else {
                        panic!("the drag must end")
                    };
                    self.preempted.push(end);
                }
                Effect::Configure => owner.configure(self)?,
                Effect::SelectionStarted(gesture) => {
                    assert_eq!(self.orbit, None);
                    assert_eq!(self.drag.live(), None);
                    assert!(self.selection_above && self.captured);
                    self.starts.push(gesture);
                }
                Effect::SelectionEnded(gesture, cause, end) => {
                    self.endings.push((gesture, cause, end));
                }
            }
        }
        Ok(())
    }

    fn enter(&mut self, owner: &mut OverlayOwner, gesture: GestureId) {
        let effects = owner.select(gesture);
        let ready = self.apply(owner, effects).map_err(|_| ());
        let seq = self
            .input
            .press(|| self.input.pressed())
            .expect("selection press is forwarded");
        let effects = owner.selected(gesture, MouseButton::Left, seq, ready);
        assert_eq!(self.apply(owner, effects), Ok(()));
    }
}

fn registered() -> OverlayOwner {
    let mut owner = OverlayOwner::default();
    assert_eq!(owner.register(A, RECT), vec![Effect::Configure]);
    owner
}

#[test]
fn regions_register_update_remove_and_reject_empty_rectangles() {
    let mut owner = OverlayOwner::default();
    let mut platform = Platform::default();
    assert_eq!(owner.configure(&mut platform), Ok(()));
    assert_eq!(owner.mode(), OverlayInput::PassThrough);
    for rect in [[0, 0, 0, 4], [0, 0, 4, 0], [0, 0, -1, 4]] {
        assert_eq!(owner.register(A, rect), vec![]);
    }
    let effects = owner.register(A, [0, 0, 1, 1]);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    let effects = owner.register(A, RECT);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    let effects = owner.register(B, [5, 6, 7, 8]);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    assert_eq!(
        platform.regions,
        BTreeMap::from([(A, RECT), (B, [5, 6, 7, 8])])
    );
    let effects = owner.remove(A);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    assert_eq!(owner.mode(), OverlayInput::Regions);
    assert_eq!(platform.regions, BTreeMap::from([(B, [5, 6, 7, 8])]));
    let effects = owner.remove(B);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    assert_eq!(owner.mode(), OverlayInput::PassThrough);
    assert_eq!(platform.regions, BTreeMap::new());
    assert_eq!(owner.remove(B), vec![]);
    assert_eq!(platform.orders, vec![REST; 6]);
}

#[test]
fn all_exit_causes_restore_both_modes_once() {
    for regions in [false, true] {
        for cause in [
            ExitCause::Release,
            ExitCause::Escape,
            ExitCause::GestureCancel,
            ExitCause::BrainLost,
            ExitCause::CaptureLost,
            ExitCause::WindowDestroyed,
            ExitCause::DisplayChanged,
            ExitCause::HoldEnded,
            ExitCause::Failed,
        ] {
            let mut owner = if regions {
                registered()
            } else {
                OverlayOwner::default()
            };
            let mut platform = Platform::default();
            platform.enter(&mut owner, HOLD);
            let effects = match cause {
                ExitCause::Release => owner.selection_event(Event::Up {
                    button: MouseButton::Left,
                    at: point(px(8.0), px(9.0)),
                }),
                ExitCause::Escape => owner.selection_event(Event::Escape),
                ExitCause::CaptureLost => owner.selection_event(Event::CaptureLost {
                    seq: 21,
                    released: None,
                }),
                _ => owner.end_selection(HOLD, cause),
            };
            assert_eq!(platform.apply(&owner, effects), Ok(()));
            assert_eq!(
                owner.mode(),
                if regions {
                    OverlayInput::Regions
                } else {
                    OverlayInput::PassThrough
                }
            );
            assert_eq!(
                platform.regions,
                if regions {
                    BTreeMap::from([(A, RECT)])
                } else {
                    BTreeMap::new()
                }
            );
            let end = if cause == ExitCause::Release {
                SelectionEnd::Released
            } else {
                SelectionEnd::Cancelled
            };
            assert_eq!(platform.endings, vec![(HOLD, cause, end)]);
            assert_eq!(platform.orders, vec![PICK, REST]);
            assert_eq!(owner.end_selection(HOLD, cause), vec![]);
            assert_eq!(owner.reconcile(), vec![]);
            assert_eq!(owner.selection(), None);
        }
    }
}

#[test]
fn cancelling_and_disconnect_race_has_only_the_first_ending() {
    for (first, late) in [
        (ExitCause::GestureCancel, ExitCause::BrainLost),
        (ExitCause::BrainLost, ExitCause::GestureCancel),
    ] {
        let mut owner = registered();
        let mut platform = Platform::default();
        platform.enter(&mut owner, HOLD);
        let effects = owner.end_selection(HOLD, first);
        assert_eq!(platform.apply(&owner, effects), Ok(()));
        assert_eq!(owner.end_selection(HOLD, late), vec![]);
        assert_eq!(
            platform.endings,
            vec![(HOLD, first, SelectionEnd::Cancelled)]
        );
        platform.enter(&mut owner, GestureId(2));
        assert_eq!(owner.end_selection(HOLD, late), vec![]);
        assert_eq!(owner.selection(), Some(GestureId(2)));
    }
}

#[test]
fn restore_uses_todays_registry_and_empty_regions_become_pass_through() {
    let mut owner = registered();
    let mut platform = Platform::default();
    platform.enter(&mut owner, HOLD);
    assert_eq!(owner.remove(A), vec![]);
    assert_eq!(owner.register(B, [11, 22, 33, 44]), vec![]);
    let effects = owner.end_selection(HOLD, ExitCause::BrainLost);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    assert_eq!(platform.mode, OverlayInput::Regions);
    assert_eq!(platform.regions, BTreeMap::from([(B, [11, 22, 33, 44])]));
    platform.enter(&mut owner, GestureId(2));
    assert_eq!(owner.remove(B), vec![]);
    let effects = owner.end_selection(GestureId(2), ExitCause::DisplayChanged);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    assert_eq!(platform.mode, OverlayInput::PassThrough);
    assert_eq!(platform.regions, BTreeMap::new());
    platform.enter(&mut owner, GestureId(3));
    assert_eq!(owner.register(A, RECT), vec![]);
    let effects = owner.end_selection(GestureId(3), ExitCause::Escape);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    assert_eq!(platform.mode, OverlayInput::Regions);
    assert_eq!(platform.regions, BTreeMap::from([(A, RECT)]));
}

#[test]
fn entering_mid_orbit_cancels_it_before_selection_and_disables_regions() {
    let mut owner = registered();
    let mut platform = Platform {
        orbit: Some(A),
        ..Platform::default()
    };
    assert!(owner.begin_orbit(A));
    assert_eq!(owner.lease_drag(), None);
    platform.enter(&mut owner, HOLD);
    assert_eq!(platform.orbit, None);
    assert_eq!(platform.regions, BTreeMap::new());
    assert_eq!(platform.starts, vec![HOLD]);
    assert!(!owner.begin_orbit(A));
    assert_eq!(owner.lease_drag(), None);
    assert_eq!(owner.select(GestureId(2)), vec![]);
    assert_eq!(owner.selected(HOLD, MouseButton::Left, 25, Ok(())), vec![]);
}

#[test]
fn preempted_drag_ends_without_user_effects_before_selection_capture_is_stamped() {
    let mut owner = registered();
    let mut platform = Platform::default();
    let lease = owner.lease_drag().expect("pointer is free");
    let press = platform
        .input
        .press(|| platform.input.pressed())
        .expect("Kumo press is forwarded");
    platform
        .drag
        .press(MouseButton::Left, press)
        .expect("no existing drag");
    assert!(!owner.begin_orbit(A));
    platform.enter(&mut owner, HOLD);
    assert_eq!(platform.preempted, vec![End::Preempted]);
    assert_eq!(platform.starts, vec![HOLD]);
    assert_eq!(press, 1);
    assert_eq!(platform.last_loss, Some((2, None)));
    let (seq, released) = platform
        .last_loss
        .expect("capture changed before selection started");
    assert_eq!(
        owner.selection_event(Event::CaptureLost { seq, released }),
        vec![]
    );
    assert_eq!(owner.selection(), Some(HOLD));
    owner.return_drag(lease);
    assert_eq!(owner.lease_drag(), None);
    let effects = owner.end_selection(HOLD, ExitCause::Escape);
    assert_eq!(platform.apply(&owner, effects), Ok(()));
    let next = owner.lease_drag().expect("selection ended");
    owner.return_drag(lease);
    assert_eq!(owner.lease_drag(), None);
    owner.return_drag(next);
    assert_eq!(owner.lease_drag(), Some(super::DragLease(3)));
}

#[test]
fn voluntary_capture_loss_waits_for_release_or_reconciliation() {
    for release in [false, true] {
        let mut owner = OverlayOwner::default();
        let mut platform = Platform::default();
        platform.enter(&mut owner, HOLD);
        assert_eq!(
            owner.selection_event(Event::CaptureLost {
                seq: 21,
                released: Some(MouseButton::Left)
            }),
            vec![]
        );
        let effects = if release {
            owner.selection_event(Event::Up {
                button: MouseButton::Left,
                at: point(px(1.0), px(2.0)),
            })
        } else {
            owner.reconcile()
        };
        assert_eq!(platform.apply(&owner, effects), Ok(()));
        assert_eq!(
            platform.endings,
            if release {
                vec![(HOLD, ExitCause::Release, SelectionEnd::Released)]
            } else {
                vec![(HOLD, ExitCause::CaptureLost, SelectionEnd::Cancelled)]
            }
        );
        assert_eq!(owner.reconcile(), vec![]);
    }
}

#[test]
fn a_failed_entry_restores_live_regions_without_starting_selection() {
    for step in [
        NativeStep::DisableRegions,
        NativeStep::Raise,
        NativeStep::Capture,
    ] {
        let mut owner = registered();
        let mut platform = Platform {
            fail: Some(step),
            ..Platform::default()
        };
        platform.enter(&mut owner, HOLD);
        assert_eq!(platform.starts, vec![]);
        assert_eq!(platform.mode, OverlayInput::Regions);
        assert_eq!(platform.regions, BTreeMap::from([(A, RECT)]));
        assert_eq!(
            (platform.selection_above, platform.captured),
            (false, false)
        );
        assert_eq!(
            platform.endings,
            vec![(HOLD, ExitCause::Failed, SelectionEnd::Cancelled)]
        );
        assert_eq!(owner.selected(HOLD, MouseButton::Left, 30, Ok(())), vec![]);
        assert_eq!(owner.end_selection(HOLD, ExitCause::BrainLost), vec![]);
    }
}

#[test]
fn hold_release_commits_only_a_rectangle_at_least_four_pixels_each_way() {
    for (rect, end) in [
        ([1, 2, 0, 0], SelectionEnd::Cancelled),
        ([1, 2, 3, 4], SelectionEnd::Cancelled),
        ([1, 2, 4, 3], SelectionEnd::Cancelled),
        ([1, 2, 4, 4], SelectionEnd::Region([1, 2, 4, 4])),
        ([1, 2, 5, 4], SelectionEnd::Region([1, 2, 5, 4])),
    ] {
        let mut owner = registered();
        let mut platform = Platform::default();
        platform.enter(&mut owner, HOLD);
        owner.rectangle(HOLD, rect);
        let effects = owner.end_selection(HOLD, ExitCause::HoldEnded);
        assert_eq!(platform.apply(&owner, effects), Ok(()));
        assert_eq!(platform.endings, vec![(HOLD, ExitCause::HoldEnded, end)]);
    }
}

#[test]
fn removing_an_orbit_cancels_it_and_normal_end_returns_the_pointer() {
    let mut owner = registered();
    assert!(!owner.begin_orbit(B));
    assert!(owner.begin_orbit(A));
    assert!(!owner.begin_orbit(A));
    owner.end_orbit(B);
    assert_eq!(owner.lease_drag(), None);
    owner.end_orbit(A);
    let lease = owner.lease_drag().expect("orbit ended");
    assert_eq!(owner.lease_drag(), None);
    owner.return_drag(lease);
    assert!(owner.begin_orbit(A));
    assert_eq!(
        owner.remove(A),
        vec![Effect::InputCancelled(A), Effect::Configure]
    );
    assert_eq!(owner.mode(), OverlayInput::PassThrough);
    assert_eq!(owner.lease_drag(), Some(super::DragLease(2)));
}

#[test]
fn release_escape_shrinking_and_stale_rectangles_keep_the_current_gesture_authoritative() {
    for (update, event, cause, end) in [
        (
            None,
            Event::Up {
                button: MouseButton::Left,
                at: point(px(1.0), px(2.0)),
            },
            ExitCause::Release,
            SelectionEnd::Region([1, 2, 4, 4]),
        ),
        (
            Some((HOLD, [1, 2, 5, 4])),
            Event::Up {
                button: MouseButton::Left,
                at: point(px(1.0), px(2.0)),
            },
            ExitCause::Release,
            SelectionEnd::Region([1, 2, 5, 4]),
        ),
        (
            None,
            Event::Escape,
            ExitCause::Escape,
            SelectionEnd::Cancelled,
        ),
        (
            Some((HOLD, [1, 2, 3, 4])),
            Event::Up {
                button: MouseButton::Left,
                at: point(px(1.0), px(2.0)),
            },
            ExitCause::Release,
            SelectionEnd::Released,
        ),
        (
            Some((GestureId(2), [8, 9, 20, 30])),
            Event::Up {
                button: MouseButton::Left,
                at: point(px(1.0), px(2.0)),
            },
            ExitCause::Release,
            SelectionEnd::Region([1, 2, 4, 4]),
        ),
    ] {
        let mut owner = registered();
        let mut platform = Platform::default();
        platform.enter(&mut owner, HOLD);
        owner.rectangle(HOLD, [1, 2, 4, 4]);
        if let Some((gesture, rect)) = update {
            owner.rectangle(gesture, rect);
        }
        let effects = owner.selection_event(event);
        assert_eq!(platform.apply(&owner, effects), Ok(()));
        assert_eq!(platform.endings, vec![(HOLD, cause, end)]);
        assert_eq!(owner.selection(), None);
    }
}
