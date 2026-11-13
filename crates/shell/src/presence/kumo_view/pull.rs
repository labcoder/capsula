//! The peek's end of a pull: the press on Kumo, the moves and releases that follow wherever the
//! pointer goes, Esc, and the native bridge's capture losses, into `Pulling`. How it fits:
//! docs/development/kumo.md, "The pull" and "The native bridge".

use gpui_kit::{
    App, Bounds, Context, DispatchPhase, Entity, EventEmitter, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, WeakEntity, Window,
};

use super::{KumoView, motion};
use crate::platform::input::{Input, input};
use crate::platform::{self, PresenceError, PresenceEvent};
use crate::presence::drag::{End, Event, Generation, Outcome};
use crate::presence::pulling::{Grab, Phase, PullSpace};

/// Where the peek's Kumo is pulled from, in physical screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::presence) struct PullSite {
    /// The line as drawn.
    pub(in crate::presence) line: [i32; 4],
    /// The dock's slot around it.
    pub(in crate::presence) slot: [i32; 4],
}

/// A drag began or ended: Esc and the peek's owner follow it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::presence) enum PullChanged {
    Grabbed(Generation),
    Ended(Generation, End),
}

pub(in crate::presence) struct LeaseReturned(pub(crate) crate::presenter::overlay_input::DragLease);

impl EventEmitter<LeaseReturned> for KumoView {}

impl EventEmitter<PullChanged> for KumoView {}

impl KumoView {
    pub(in crate::presence) fn set_site(&mut self, site: Option<PullSite>) {
        self.site = site;
    }

    /// The shell is quitting: a live drag ends now, and no grab starts again.
    pub(in crate::presence) fn close(&mut self, cx: &mut Context<'_, Self>) {
        self.closing = true;
        self.pull_event(Event::SourceClosed, cx);
    }

    pub(in crate::presence) fn pull_live(&self) -> bool {
        matches!(self.pulling.phase(), Phase::Live(_))
    }

    /// The live drag, once its neck has snapped: the carry.
    pub(in crate::presence) fn carrying(&self) -> Option<Generation> {
        let Phase::Live(generation) = self.pulling.phase() else {
            return None;
        };
        let (pull, _) = self.pulling.pull()?;
        pull.neck().broken().then_some(generation)
    }

    /// `at`, a point in the peek, in physical screen pixels at the pull's scale.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "a rounded screen offset fits an i32"
    )]
    pub(in crate::presence) fn screen_point(&self, at: Point<Pixels>) -> Option<[i32; 2]> {
        let (_, space) = self.pulling.pull()?;
        let offset = |value: Pixels| (f32::from(value) * space.scale).round() as i32;
        Some([self.origin[0] + offset(at.x), self.origin[1] + offset(at.y)])
    }

    /// A press on Kumo, read while the native bridge forwards it, so its stamp orders it
    /// against capture losses. A press nothing stamped can't be ordered, so it starts nothing.
    fn press(
        &mut self,
        event: &MouseDownEvent,
        centre: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (Some(seq), Some(site)) = (input(Input::pressed), self.site) else {
            return;
        };
        let placed =
            platform::work_area().and_then(|area| Ok((area, platform::client_origin(window)?)));
        let (area, origin) = match placed {
            Ok(placed) => placed,
            Err(error) => {
                tracing::warn!(%error, "can't place Kumo's pull");
                return;
            }
        };
        let space = PullSpace {
            area,
            scale: window.scale_factor(),
        };
        let home = space.point(origin, centre);
        let grab = Grab::new(space, site.line, site.slot, home, motion(cx));
        let Some(owner) = &self.owner else { return };
        let Ok(Some(lease)) = owner.update(cx, |owner, _| owner.presenter.input.lease_drag())
        else {
            return;
        };
        self.lease = Some(lease);
        self.grab(event.button, seq, origin, event.position, grab, cx);
        if !self.pull_live()
            && let Some(lease) = self.lease.take()
        {
            cx.emit(LeaseReturned(lease));
        }
    }

    /// Starts a pull at `at` in the peek whose client area starts at `origin`, unless one exists.
    pub(super) fn grab(
        &mut self,
        button: MouseButton,
        seq: u64,
        origin: [i32; 2],
        at: Point<Pixels>,
        grab: Grab,
        cx: &mut Context<'_, Self>,
    ) {
        if self.closing {
            return;
        }
        if let Some(generation) = self.pulling.press(button, seq, origin, at, grab) {
            self.origin = origin;
            cx.emit(PullChanged::Grabbed(generation));
        }
    }

    /// A move or a release while the drag may be live, with the work area and scale read as it
    /// arrived. Windows posts nothing the peek follows when they change, so each is checked
    /// first: a change, or a read that failed, loses the drag.
    fn pointer(
        &mut self,
        event: Event,
        now: impl FnOnce() -> Result<PullSpace, PresenceError>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.pull_live() {
            return;
        }
        let outcome = match now() {
            Ok(space) => self.pulling.rescale(space),
            Err(error) => {
                tracing::debug!(%error, "can't read the work area mid-pull");
                self.pulling.handle(Event::SourceClosed, self.origin)
            }
        };
        self.follow(outcome, cx);
        self.pull_event(event, cx);
    }

    /// One event for the drag.
    pub(in crate::presence) fn pull_event(&mut self, event: Event, cx: &mut Context<'_, Self>) {
        let outcome = self.pulling.handle(event, self.origin);
        self.follow(outcome, cx);
    }

    /// A presence window lost capture. The drag decides whether the loss ends it; one its own
    /// button's release caused is reconciled only after that release's forward has returned.
    fn capture_lost(
        &mut self,
        seq: u64,
        released: Option<MouseButton>,
        cx: &mut Context<'_, Self>,
    ) {
        self.pull_event(Event::CaptureLost { seq, released }, cx);
        if input(Input::reconcile_after_release) {
            let view = cx.weak_entity();
            cx.defer(move |cx| update(&view, cx, Self::reconcile));
        }
    }

    /// The release that took capture has been forwarded: a drag still waiting for it is lost.
    fn reconcile(&mut self, cx: &mut Context<'_, Self>) {
        let outcome = self.pulling.reconcile();
        self.follow(outcome, cx);
    }

    fn follow(&mut self, outcome: Outcome, cx: &mut Context<'_, Self>) {
        if let Outcome::Ended(generation, end) = outcome {
            if let Some(lease) = self.lease.take() {
                cx.emit(LeaseReturned(lease));
            }
            cx.emit(PullChanged::Ended(generation, end));
        }
    }
}

/// The native bridge's capture losses and reconciles, from the presence channel.
pub(in crate::presence) fn bridge(kumo: &Entity<KumoView>, event: PresenceEvent, cx: &mut App) {
    kumo.update(cx, |kumo, cx| match event {
        PresenceEvent::CaptureLost { seq, released } => kumo.capture_lost(seq, released, cx),
        PresenceEvent::Reconcile => kumo.reconcile(cx),
        _ => {}
    });
}

/// The pull's listeners for the next frame. The press must land on Kumo. Moves and releases
/// count wherever they land, since capture keeps them in this window for the whole drag, and
/// GPUI's element listeners hear only the pointer over their own hitbox.
pub(super) fn listen(bounds: Bounds<Pixels>, view: WeakEntity<KumoView>, window: &mut Window) {
    let pressed = view.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
            return;
        }
        if bounds.contains(&event.position) {
            update(&pressed, cx, |view, cx| {
                view.press(event, bounds.center(), window, cx);
            });
        }
        #[cfg(test)]
        update(&pressed, cx, |view, _| view.pressed_square = Some(bounds));
        // Numbers only, for the gate: where the press landed against Kumo's square.
        let grabbed = pressed
            .upgrade()
            .is_some_and(|view| view.read(cx).pull_live());
        crate::presence::diagnostics::record(
            cx,
            format_args!(
                "kumo press at={},{} bounds={},{},{},{} grabbed={grabbed}",
                f32::from(event.position.x),
                f32::from(event.position.y),
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            ),
        );
    });
    let moved = view.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Capture {
            let event = Event::Move {
                at: event.position,
                pressed: event.pressed_button,
            };
            update(&moved, cx, |view, cx| {
                view.pointer(event, || space(window), cx);
            });
        }
    });
    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
        if phase == DispatchPhase::Capture {
            let up = Event::Up {
                button: event.button,
                at: event.position,
            };
            update(&view, cx, |view, cx| view.pointer(up, || space(window), cx));
        }
    });
}

/// The work area and scale now.
fn space(window: &Window) -> Result<PullSpace, PresenceError> {
    Ok(PullSpace {
        area: platform::work_area()?,
        scale: window.scale_factor(),
    })
}

fn update(
    view: &WeakEntity<KumoView>,
    cx: &mut App,
    f: impl FnOnce(&mut KumoView, &mut Context<'_, KumoView>),
) {
    if view.update(cx, f).is_err() {
        tracing::debug!("the peek closed under the pointer");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use async_channel::Sender;
    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, Entity, MouseButton, point, px};

    use super::{KumoView, PullChanged};
    use crate::platform::input::input;
    use crate::platform::{PresenceError, PresenceEvent};
    use crate::presence::drag::{End, Event};
    use crate::presence::kumo::{Motion, v2};
    use crate::presence::pulling::{Grab, Phase, PullSpace};

    const LEFT: MouseButton = MouseButton::Left;

    fn grab(view: &Entity<KumoView>, cx: &mut TestAppContext) {
        let grab = Grab::new(
            SPACE,
            [612, 1000, 56, 3],
            [608, 990, 64, 24],
            v2(640.0, 920.0),
            Motion::Reduced,
        );
        let seq = input(|input| input.press(|| input.pressed())).expect("a forwarded press");
        view.update(cx, |view, cx| {
            view.grab(LEFT, seq, [560, 860], point(px(80.0), px(60.0)), grab, cx);
        });
    }

    /// A peek with a live drag on the left button, the changes it emits, and the presence
    /// channel, drained through `bridge` as the presence drains it.
    fn grabbed(
        cx: &mut TestAppContext,
    ) -> (
        Entity<KumoView>,
        Rc<RefCell<Vec<PullChanged>>>,
        Sender<PresenceEvent>,
    ) {
        let view = cx.new(|_| KumoView::new());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&seen);
        cx.update(|cx| {
            cx.subscribe(&view, move |_, changed: &PullChanged, _| {
                log.borrow_mut().push(*changed);
            })
            .detach();
        });
        let (events, drained) = async_channel::unbounded();
        let kumo = view.clone();
        cx.update(|cx| {
            cx.spawn(async move |cx| {
                while let Ok(event) = drained.recv().await {
                    cx.update(|cx| super::bridge(&kumo, event, cx));
                }
            })
            .detach();
        });
        grab(&view, cx);
        assert_eq!(seen.borrow().len(), 1, "the press grabbed");
        (view, seen, events)
    }

    /// Forwards the left button's release with a nested forward inside it, as a modal loop
    /// pumping the queue would, and drains the release's capture loss inside that nested
    /// forward. `up` reaches GPUI before the outer forward returns. Returns whether the
    /// reconcile is due, after checking that nothing ended early.
    fn release(
        cx: &mut TestAppContext,
        view: &Entity<KumoView>,
        seen: &Rc<RefCell<Vec<PullChanged>>>,
        events: &Sender<PresenceEvent>,
        up: Option<Event>,
    ) -> bool {
        let ((), due) = input(|input| {
            input.release(LEFT, || {
                let (seq, released) = input.capture_changed();
                assert_eq!(released, Some(LEFT), "the release took capture");
                events
                    .try_send(PresenceEvent::CaptureLost { seq, released })
                    .expect("the channel is open");
                let ((), inner_due) = input.release(MouseButton::Middle, || cx.run_until_parked());
                assert!(!inner_due, "an inner forward doesn't reconcile");
                assert_eq!(
                    input.capture_changed().1,
                    Some(LEFT),
                    "the outer release is restored after the inner one"
                );
                assert_eq!(seen.borrow().len(), 1, "the loss is held for the release");
                if let Some(up) = up {
                    view.update(cx, |view, cx| view.pull_event(up, cx));
                }
            })
        });
        due
    }

    #[gpui::test]
    fn a_release_drained_inside_a_nested_forward_ends_once_as_released(cx: &mut TestAppContext) {
        let (view, seen, events) = grabbed(cx);
        let at = point(px(300.0), px(-40.0));
        let up = Event::Up { button: LEFT, at };
        assert!(release(cx, &view, &seen, &events, Some(up)));
        events
            .try_send(PresenceEvent::Reconcile)
            .expect("the channel is open");
        cx.run_until_parked();
        let PullChanged::Grabbed(generation) = seen.borrow()[0] else {
            panic!("the first change is the grab");
        };
        assert_eq!(
            *seen.borrow(),
            [
                PullChanged::Grabbed(generation),
                PullChanged::Ended(generation, End::Released(at)),
            ]
        );
    }

    #[gpui::test]
    fn a_release_that_never_reaches_gpui_is_lost_only_after_the_outer_forward(
        cx: &mut TestAppContext,
    ) {
        let (view, seen, events) = grabbed(cx);
        assert!(release(cx, &view, &seen, &events, None));
        assert_eq!(seen.borrow().len(), 1, "nothing ends before the reconcile");
        events
            .try_send(PresenceEvent::Reconcile)
            .expect("the channel is open");
        cx.run_until_parked();
        let PullChanged::Grabbed(generation) = seen.borrow()[0] else {
            panic!("the first change is the grab");
        };
        assert_eq!(
            *seen.borrow(),
            [
                PullChanged::Grabbed(generation),
                PullChanged::Ended(generation, End::Lost),
            ]
        );
        assert!(view.read_with(cx, |view, _| !view.pull_live()));
    }

    /// The grab's space, as `grab` made it.
    const SPACE: PullSpace = PullSpace {
        area: [0, 0, 1280, 1024],
        scale: 1.0,
    };

    /// The changes a release brings when the work area and scale read as `now` at the Up,
    /// with no move before it.
    fn release_reading(cx: &mut TestAppContext, now: Result<PullSpace, PresenceError>) -> End {
        let (view, seen, _) = grabbed(cx);
        let at = point(px(300.0), px(-40.0));
        view.update(cx, |view, cx| {
            view.pointer(Event::Up { button: LEFT, at }, || now, cx);
        });
        let changes = seen.borrow();
        let [
            PullChanged::Grabbed(generation),
            PullChanged::Ended(ended, end),
        ] = changes[..]
        else {
            panic!("one grab and one ending, not {changes:?}");
        };
        assert_eq!(ended, generation);
        end
    }

    #[gpui::test]
    fn an_up_in_the_grabs_space_is_released(cx: &mut TestAppContext) {
        assert_eq!(
            release_reading(cx, Ok(SPACE)),
            End::Released(point(px(300.0), px(-40.0)))
        );
    }

    #[gpui::test]
    fn an_up_after_the_work_area_changed_with_no_move_is_lost(cx: &mut TestAppContext) {
        let taskbar = PullSpace {
            area: [0, 0, 1280, 984],
            ..SPACE
        };
        assert_eq!(release_reading(cx, Ok(taskbar)), End::Lost);
        let scaled = PullSpace {
            scale: 1.25,
            ..SPACE
        };
        assert_eq!(release_reading(cx, Ok(scaled)), End::Lost);
    }

    #[gpui::test]
    fn an_up_whose_work_area_cant_be_read_is_lost(cx: &mut TestAppContext) {
        assert_eq!(release_reading(cx, Err(PresenceError::NotWin32)), End::Lost);
    }

    #[gpui::test]
    fn quitting_loses_a_live_drag_once_and_refuses_a_grab_on_an_idle_peek(cx: &mut TestAppContext) {
        let (view, seen, _) = grabbed(cx);
        view.update(cx, KumoView::close);
        view.update(cx, KumoView::close);
        view.update(cx, |view, _| {
            assert!(view.pulling.step(1.0 / 72.0).is_some());
            assert!(view.pulling.rested(), "reduced motion is home in one step");
            assert_eq!(view.pulling.phase(), Phase::Idle);
        });
        grab(&view, cx);
        let PullChanged::Grabbed(generation) = seen.borrow()[0] else {
            panic!("the first change is the grab");
        };
        assert_eq!(
            *seen.borrow(),
            [
                PullChanged::Grabbed(generation),
                PullChanged::Ended(generation, End::Lost),
            ],
            "only the closed peek refused the second grab"
        );
        assert!(view.read_with(cx, |view, _| !view.pull_live()));
    }
}
