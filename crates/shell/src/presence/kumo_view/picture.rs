//! The peek's end of the pulled picture: each frame of a pull becomes the dock's next picture,
//! and the body leaves the peek's slot only once the dock shows it. How it fits:
//! docs/development/kumo.md, "The pulled picture".

use gpui_kit::Context;

use super::{KumoView, Pulled, WantsFrames};
use crate::presence::pull_picture::{PullPicture, Rows};

impl KumoView {
    /// Steps the pull, if there is one, and makes the dock's next picture of it. Once the
    /// released body is home and still, it rests instead: the next picture is the line alone.
    pub(super) fn picture(&mut self, dt: f32) {
        if self.pulling.step(dt).is_none() {
            return;
        }
        let Some((pull, space)) = self.pulling.pull() else {
            return;
        };
        if pull.settled() {
            self.pulled = Pulled::Resting;
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

    /// The dock's next picture, the pixels it can show, and those of the picture shown now,
    /// which the mask keeps owning until the next one is drawn.
    pub(in crate::presence) fn next_picture(&self) -> Option<(&PullPicture, &Rows, &Rows)> {
        Some((self.next.as_ref()?, &self.next_rows, &self.shown_rows))
    }

    /// The dock accepted the next picture's mask and shows it now, so the body leaves the peek.
    pub(in crate::presence) fn shown(&mut self, cx: &mut Context<'_, Self>) -> Option<PullPicture> {
        let picture = self.next.take()?;
        std::mem::swap(&mut self.next_rows, &mut self.shown_rows);
        if self.pulled == Pulled::Peek {
            self.pulled = Pulled::Dock;
            cx.notify();
        }
        Some(picture)
    }

    /// The released body is home and still, waiting for the dock to show the line alone.
    pub(in crate::presence) fn resting(&self) -> bool {
        self.pulled == Pulled::Resting
    }

    /// The dock accepted the line alone: the pull is over and the peek gets its body back.
    /// Returns whether it was resting.
    pub(in crate::presence) fn rested(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if self.pulled != Pulled::Resting || !self.pulling.rested() {
            return false;
        }
        self.pulled = Pulled::Peek;
        self.shown_rows.clear(0, 0);
        cx.notify();
        if self.wants_frame() {
            cx.emit(WantsFrames);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, Entity, MouseButton, point, px};

    use super::{KumoView, Pulled};
    use crate::platform::input::input;
    use crate::presence::drag::Event;
    use crate::presence::kumo::{Look, Motion, Renderer, Silhouette, v2};
    use crate::presence::pull_picture::PullArt;
    use crate::presence::pulling::{Grab, Phase, PullSpace};

    const SPACE: PullSpace = PullSpace {
        area: [0, 0, 1280, 1024],
        scale: 1.0,
    };

    /// A peek with the pulled art baked, grabbed under the line at the bottom of the work area.
    fn grabbed(cx: &mut TestAppContext, motion: Motion) -> Entity<KumoView> {
        let view = cx.new(|_| {
            let mut view = KumoView::new();
            let renderer = Renderer::new(&Silhouette::emoji(), 84, Look::default());
            view.art = Some(
                PullArt::new(&renderer, 84, Silhouette::emoji(), Look::default())
                    .expect("the art bakes"),
            );
            view
        });
        let grab = Grab::new(
            SPACE,
            [612, 1000, 56, 3],
            [608, 990, 64, 24],
            v2(640.0, 920.0),
            motion,
        );
        let seq = input(|input| input.press(|| input.pressed())).expect("a forwarded press");
        view.update(cx, |view, cx| {
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

    #[gpui::test]
    fn a_pulled_frame_draws_the_body_inside_the_pull_area_and_owns_its_pixels(
        cx: &mut TestAppContext,
    ) {
        let view = grabbed(cx, Motion::Reduced);
        view.update(cx, |view, _| {
            view.picture(1.0 / 72.0);
            let (picture, rows, _) = view.next_picture().expect("a picture");
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
    fn a_full_motion_pull_draws_its_neck_under_the_body(cx: &mut TestAppContext) {
        let view = grabbed(cx, Motion::Full);
        view.update(cx, |view, cx| {
            let at = point(px(140.0), px(-20.0));
            view.pull_event(
                Event::Move {
                    at,
                    pressed: Some(MouseButton::Left),
                },
                cx,
            );
            for _ in 0..6 {
                view.picture(1.0 / 72.0);
            }
            let (picture, _, _) = view.next_picture().expect("a picture");
            assert!(picture.images[0].is_some(), "the attached neck is drawn");
            assert!(picture.images[2].is_some(), "and the body over it");
        });
    }

    #[gpui::test]
    fn a_rested_body_stays_out_until_the_dock_accepts_the_line_alone(cx: &mut TestAppContext) {
        let view = grabbed(cx, Motion::Reduced);
        view.update(cx, |view, cx| {
            view.picture(1.0 / 72.0);
            assert!(view.shown(cx).is_some());
            assert_eq!(
                view.pulled,
                Pulled::Dock,
                "the dock shows the body, the peek doesn't"
            );
            let up = Event::Up {
                button: MouseButton::Left,
                at: point(px(80.0), px(60.0)),
            };
            view.pull_event(up, cx);
            view.picture(1.0 / 72.0);
            assert!(view.resting(), "reduced motion is home in one step");
            assert!(
                !view.wants_frame(),
                "nothing to step while the line is uploaded"
            );
            // The line's upload failed: the next frame changes nothing.
            view.picture(1.0 / 72.0);
            assert_eq!(view.pulling.phase(), Phase::Returning);
            assert_eq!(
                view.pulled,
                Pulled::Resting,
                "the body stays out of the peek"
            );
            assert!(view.rested(cx), "the retry was accepted");
            assert_eq!(view.pulling.phase(), Phase::Idle);
            assert_eq!(view.pulled, Pulled::Peek);
            assert!(!view.rested(cx), "it rests once");
        });
    }
}
