//! The peek's end of the pulled picture. The dock's frames step the pull and make each next
//! picture, since the peek may hide once the drag ends; the body leaves the peek's slot only
//! once the dock has drawn it. How it fits: docs/development/kumo.md, "The pulled picture".

use gpui_kit::Context;

use super::{KumoView, Pulled, WantsFrames};
use crate::presence::pull_picture::{PullPicture, Rows};
use crate::presence::pulling::Phase;

impl KumoView {
    /// One of the dock's pull frames: steps the pull, if there is one, and makes the dock's next
    /// picture of it. Once the released body is home and still, it rests instead.
    pub(in crate::presence) fn pull_step(&mut self, dt: f64) {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "a frame step is at most two intervals"
        )]
        self.picture(dt as f32);
    }

    fn picture(&mut self, dt: f32) {
        if self.pulling.step(dt).is_none() {
            return;
        }
        let Some((pull, space)) = self.pulling.pull() else {
            return;
        };
        if pull.settled() {
            self.resting = true;
            self.next = None;
            return;
        }
        let Some(art) = self.art.as_mut() else {
            return;
        };
        self.next = art
            .frame(pull, space, &mut self.next_rows)
            .inspect_err(|error| tracing::warn!(%error, "can't draw the pulled Kumo"))
            .ok();
    }

    /// Whether the pull wants another of the dock's frames.
    pub(in crate::presence) fn pull_wants_frame(&self) -> bool {
        self.pulling.phase() != Phase::Idle && !self.resting
    }

    /// The dock's next picture, and the pixels it can show.
    pub(in crate::presence) fn next_picture(&mut self) -> Option<(&PullPicture, &mut Rows)> {
        Some((self.next.as_ref()?, &mut self.next_rows))
    }

    /// The dock accepted the next picture's mask: it's the dock's to draw now.
    pub(in crate::presence) fn take_picture(&mut self) -> Option<PullPicture> {
        self.next.take()
    }

    /// The dock has drawn a pulled picture, so the body leaves the peek's slot.
    pub(in crate::presence) fn dock_drew(&mut self, cx: &mut Context<'_, Self>) {
        if self.pulled == Pulled::Peek && self.pulling.phase() != Phase::Idle {
            self.pulled = Pulled::Dock;
            cx.notify();
        }
    }

    /// The released body is home and still, waiting for the dock to show the line alone.
    pub(in crate::presence) fn resting(&self) -> bool {
        self.resting
    }

    /// The dock accepted the line alone: the pull is over and the peek shows the body. Returns
    /// whether it was resting.
    pub(in crate::presence) fn rested(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.resting || !self.pulling.rested() {
            return false;
        }
        self.resting = false;
        self.dock_released(cx);
        true
    }

    /// The dock no longer draws the body, so the peek does.
    pub(in crate::presence) fn dock_released(&mut self, cx: &mut Context<'_, Self>) {
        self.pulled = Pulled::Peek;
        cx.notify();
        if self.wants_frame() {
            cx.emit(WantsFrames);
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, Entity, MouseButton, point, px};

    use super::{KumoView, Pulled};
    use crate::platform::input::input;
    use crate::presence::drag::Event;
    use crate::presence::kumo::{Motion, v2};
    use crate::presence::kumo_view::pull_rows;
    use crate::presence::pulling::{Grab, Phase, PullSpace};

    const SPACE: PullSpace = PullSpace {
        area: [0, 0, 1280, 1024],
        scale: 1.0,
    };
    const STEP: f64 = 1.0 / 72.0;

    /// A peek baked at 100%, grabbed under the line at the bottom of the work area.
    fn grabbed(cx: &mut TestAppContext, motion: Motion) -> Entity<KumoView> {
        let view = cx.new(|_| KumoView::new());
        view.update(cx, |view, cx| view.rebake(1.0, cx));
        cx.run_until_parked();
        let grab = Grab::new(
            SPACE,
            [612, 1000, 56, 3],
            [608, 990, 64, 24],
            v2(640.0, 920.0),
            motion,
        );
        let seq = input(|input| input.press(|| input.pressed())).expect("a forwarded press");
        view.update(cx, |view, cx| {
            assert!(view.art.is_some(), "the pulled art baked");
            view.grab(
                MouseButton::Left,
                seq,
                [560, 860],
                point(px(80.0), px(60.0)),
                grab,
                cx,
            );
        });
        view
    }

    fn release(view: &mut KumoView, cx: &mut gpui_kit::Context<'_, KumoView>) {
        let up = Event::Up {
            button: MouseButton::Left,
            at: point(px(80.0), px(60.0)),
        };
        view.pull_event(up, cx);
    }

    #[gpui::test]
    fn a_pulled_frame_draws_the_body_inside_the_pull_area_and_owns_its_pixels(
        cx: &mut TestAppContext,
    ) {
        let view = grabbed(cx, Motion::Reduced);
        view.update(cx, |view, _| {
            let reserved = view.next_rows.capacity();
            assert!(reserved >= pull_rows(1.0), "the bake sized the rows");
            view.pull_step(STEP);
            let (picture, rows) = view.next_picture().expect("a picture");
            assert_eq!(rows.capacity(), reserved, "the first frame grew nothing");
            assert_eq!(
                picture.area,
                [380, 762, 520, 262],
                "the slot grown by 228, cut at 1024"
            );
            assert!(
                picture.images[..2].iter().all(Option::is_none),
                "reduced draws no goo"
            );
            let (body, _) = picture.images[2].as_ref().expect("the body");
            let [x, y, width, height] = picture.area;
            assert!(body.x >= x && body.x + body.width <= x + width);
            assert!(body.y >= y && body.y + body.height <= y + height);
            let owned: Vec<_> = (y..y + height)
                .flat_map(|row| {
                    rows.row(row)
                        .into_iter()
                        .flatten()
                        .map(move |span| (row, span))
                })
                .collect();
            assert!(!owned.is_empty(), "the body's pixels are owned");
            assert!(owned.iter().all(|(row, [left, right])| {
                (body.y - 1..=body.y + body.height).contains(row)
                    && *left >= body.x - 1
                    && *right <= body.x + body.width + 1
            }));
        });
    }

    #[gpui::test]
    fn a_full_motion_pull_puts_its_neck_in_a_slot_before_the_body_without_growing_scratch(
        cx: &mut TestAppContext,
    ) {
        let view = grabbed(cx, Motion::Full);
        view.update(cx, |view, cx| {
            let scratch = view.art.as_ref().expect("baked").scratch_capacity();
            let at = point(px(140.0), px(-20.0));
            view.pull_event(
                Event::Move {
                    at,
                    pressed: Some(MouseButton::Left),
                },
                cx,
            );
            for _ in 0..6 {
                view.pull_step(STEP);
            }
            let (picture, _) = view.next_picture().expect("a picture");
            assert!(picture.images[0].is_some(), "the attached neck is drawn");
            assert!(picture.images[2].is_some(), "and the body after it");
            assert_eq!(
                view.art.as_ref().expect("baked").scratch_capacity(),
                scratch,
                "the goo's scratch was sized at the bake"
            );
        });
    }

    #[gpui::test]
    fn a_body_the_dock_drew_stays_out_until_the_dock_lets_go_and_the_pull_rests_once(
        cx: &mut TestAppContext,
    ) {
        let view = grabbed(cx, Motion::Reduced);
        view.update(cx, |view, cx| {
            view.pull_step(STEP);
            assert!(view.take_picture().is_some());
            assert_eq!(view.pulled, Pulled::Peek, "accepted isn't drawn yet");
            view.dock_drew(cx);
            assert_eq!(view.pulled, Pulled::Dock, "the dock drew the body");
            release(view, cx);
            view.pull_step(STEP);
            assert!(view.resting(), "reduced motion is home in one step");
            assert!(
                !view.pull_wants_frame(),
                "nothing to step while the line uploads"
            );
            view.pull_step(STEP);
            assert_eq!(view.pulling.phase(), Phase::Returning);
            assert_eq!(view.pulled, Pulled::Dock, "the dock still shows the body");
            view.dock_released(cx);
            assert_eq!(view.pulled, Pulled::Peek, "the dock let go of it");
            assert_eq!(
                view.pulling.phase(),
                Phase::Returning,
                "until the line is back"
            );
            assert!(view.rested(cx));
            assert_eq!(view.pulling.phase(), Phase::Idle);
            assert_eq!(view.pulled, Pulled::Peek);
            assert!(!view.rested(cx), "it rests once");
        });
    }

    #[gpui::test]
    fn a_shown_peek_shades_nothing_while_the_dock_draws_the_body(cx: &mut TestAppContext) {
        let view = grabbed(cx, Motion::Full);
        view.update(cx, |view, cx| {
            view.show(Motion::Full, cx);
            assert!(view.wants_frame(), "the peek blinks");
            view.pull_step(STEP);
            assert!(view.take_picture().is_some());
            view.dock_drew(cx);
            assert!(!view.wants_frame(), "the dock draws the body");
            view.dock_released(cx);
            assert!(view.wants_frame(), "the peek draws it again");
        });
    }

    #[gpui::test]
    fn a_body_the_dock_never_accepted_stays_in_the_peek_while_it_rests(cx: &mut TestAppContext) {
        let view = grabbed(cx, Motion::Reduced);
        view.update(cx, |view, cx| {
            view.pull_step(STEP);
            assert!(
                view.next_picture().is_some(),
                "its upload failed: never taken"
            );
            release(view, cx);
            view.pull_step(STEP);
            assert!(view.resting());
            assert_eq!(view.pulled, Pulled::Peek, "the peek still shows the body");
            view.dock_drew(cx);
            assert!(view.rested(cx));
            assert_eq!(view.pulled, Pulled::Peek);
        });
    }

    #[gpui::test]
    fn a_return_reaches_rest_on_the_docks_frames_with_the_peek_hidden(cx: &mut TestAppContext) {
        let view = grabbed(cx, Motion::Full);
        view.update(cx, |view, cx| {
            view.pull_event(
                Event::Move {
                    at: point(px(80.0), px(-200.0)),
                    pressed: Some(MouseButton::Left),
                },
                cx,
            );
            for _ in 0..20 {
                view.pull_step(STEP);
            }
            release(view, cx);
            view.hide(cx);
            assert!(!view.wants_frame(), "the hidden peek asks for nothing");
            let steps = (0..400)
                .take_while(|_| {
                    view.pull_step(STEP);
                    view.pull_wants_frame()
                })
                .count();
            assert!(steps < 399, "the return settled on the dock's frames");
            assert!(view.resting());
        });
    }
}
