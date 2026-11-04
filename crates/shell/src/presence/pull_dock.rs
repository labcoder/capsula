//! The dock's end of the pulled picture. The dock's window, shown through the whole pull and
//! its return, paces the pull's frames, since the peek may hide once the drag ends. Each
//! frame the overlay covers the pull area, the mask owns the line, the next picture and the
//! one the dock shows, and the dock draws the next picture only once Windows accepts that
//! mask, one picture per dock draw. Once the body rests, the line takes the dock back. How it
//! fits: docs/development/kumo.md, "The pulled picture".

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::{App, Context, RenderImage, Window};

use super::frame_rate::FrameLoop;
use super::kumo_view::KumoView;
use super::line::{LineFrame, on_next_frame};
use super::observer::{self, Drawn};
use super::pull_picture::{PullPicture, RowFull, Rows};
use super::{Presence, platform};

/// Whether the dock has rendered the picture it accepted last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Drawing {
    Drawn,
    /// It may not be on screen yet.
    Pending,
}

/// What one dock frame did with the next picture.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Commit<E> {
    /// The dock hasn't drawn the picture it accepted last, so nothing new is uploaded.
    Waiting,
    /// Windows holds the mask, so the dock draws the picture now.
    Accepted,
    /// More spans than a mask row holds; the dock keeps its picture.
    Full(RowFull),
    /// Windows refused the mask; the dock keeps its picture.
    Failed(E),
}

/// The dock's state during a pull. Its rows are sized once, so a frame grows none.
#[derive(Debug, Default)]
pub(super) struct PullDock {
    frames: FrameLoop,
    /// Where the overlay is, while a pull holds it.
    area: Option<[i32; 4]>,
    /// The pixels of the picture the dock accepted last, which it draws or has drawn.
    shown: Rows,
    /// The next upload's rows.
    mask: Rows,
    /// The line has been asked to take the dock back.
    reclaiming: bool,
}

impl PullDock {
    /// Rows for a pull area up to `rows` tall.
    pub(super) fn new(rows: usize) -> Self {
        Self {
            shown: Rows::with_capacity(rows),
            mask: Rows::with_capacity(rows),
            ..Self::default()
        }
    }

    /// Uploads the mask for the next picture, `area` with pixels `next`, over `line`:
    /// the line's band, `next` and the picture shown now. Once accepted, `next` is shown.
    /// While the last accepted picture's drawing is pending, nothing is uploaded: the mask
    /// must keep owning it.
    pub(super) fn commit<E>(
        &mut self,
        drawing: Drawing,
        line: &LineFrame,
        area: [i32; 4],
        next: &mut Rows,
        upload: impl FnOnce([i32; 4], &Rows) -> Result<(), E>,
    ) -> Commit<E> {
        if drawing == Drawing::Pending {
            return Commit::Waiting;
        }
        let mask = &mut self.mask;
        mask.clear(area[1], usize::try_from(area[3]).unwrap_or(0));
        let [_, top, _, height] = line.hit;
        let owned = (top..top + height)
            .try_for_each(|y| {
                line.row_spans(y)
                    .into_iter()
                    .flatten()
                    .try_for_each(|span| mask.push(y, span))
            })
            .and_then(|()| mask.extend(next))
            .and_then(|()| mask.extend(&self.shown));
        if let Err(full) = owned {
            return Commit::Full(full);
        }
        if let Err(error) = upload(area, &self.mask) {
            return Commit::Failed(error);
        }
        std::mem::swap(next, &mut self.shown);
        Commit::Accepted
    }
}

impl Presence {
    /// Arms the pull's frames on the dock's window, unless they're armed.
    pub(super) fn want_pull_frames(&mut self, cx: &mut Context<'_, Self>) {
        if !self.pull_dock.frames.want(Instant::now()) {
            return;
        }
        let owner = cx.weak_entity();
        let armed = self.dock_window.update(cx, |_, window, _| {
            on_next_frame(window, owner, Self::pull_frame);
        });
        if let Err(error) = armed {
            self.pull_dock.frames.idle();
            tracing::warn!(%error, "can't step Kumo's pull");
        }
    }

    /// One of the dock's frames during a pull: the body leaves the peek once the dock has
    /// drawn it, the pull steps, and the next picture is committed.
    fn pull_frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let cap = self
            .settings
            .read(cx)
            .view()
            .map(|view| view.max_frame_rate)
            .unwrap_or_default();
        let paced = self
            .pull_dock
            .frames
            .frame(Instant::now(), cap, window.refresh_interval(cx));
        if let Some(dt) = paced {
            let drew = self.dock.read(cx).drew_pull();
            self.kumo.update(cx, |kumo, cx| {
                if drew {
                    kumo.dock_drew(cx);
                }
                kumo.pull_step(dt);
            });
            self.show_pull(window, cx);
        }
        if paced.is_none() || self.kumo.read(cx).pull_wants_frame() || self.dock.read(cx).undrawn {
            on_next_frame(window, cx.weak_entity(), Self::pull_frame);
        } else {
            self.pull_dock.frames.idle();
        }
    }

    /// Commits the pull's next picture, or once the body rests, asks the line to take the
    /// dock back; the pull ends only once that's accepted.
    fn show_pull(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.kumo.read(cx).resting() {
            if !self.pull_dock.reclaiming {
                self.pull_dock.reclaiming = true;
                self.line.reclaim();
                self.try_line(cx.weak_entity(), window, cx);
            }
            return;
        }
        let Some(line) = self.line.accepted_frame() else {
            return;
        };
        let Some(area) = self.kumo.update(cx, |kumo, _| {
            kumo.next_picture().map(|(picture, _)| picture.area)
        }) else {
            return;
        };
        if self.pull_dock.area != Some(area) {
            self.line.pull_overlay();
            if let Err(error) = platform::place_presence_window(window, area) {
                tracing::debug!(%error, "can't grow the dock for the pull");
                return;
            }
            window.bounds_changed(cx);
            self.pull_dock.area = Some(area);
            self.dock.update(cx, |dock, cx| {
                dock.origin = [area[0], area[1]];
                cx.notify();
            });
        }
        let drawing = if self.dock.read(cx).undrawn {
            Drawing::Pending
        } else {
            Drawing::Drawn
        };
        let mut drawn = self.drawn;
        let commit = self.kumo.update(cx, |kumo, cx| {
            let Some((_, next)) = kumo.next_picture() else {
                return Commit::Waiting;
            };
            self.pull_dock
                .commit(drawing, &line, area, next, |area, mask| {
                    // shortcut: the observer records a pull's commits without their geometry; the
                    // measurement layer gives it the line, the body and their owned spans.
                    let begin_qpc = observer::begin(cx);
                    let update = self.native.reshape(area, |y| mask.row(y));
                    drawn = observer::commit(cx, begin_qpc, None, area, &update);
                    update.map(|_| self.line.pull_mask(area))
                })
        });
        match commit {
            Commit::Accepted => {
                self.drawn = drawn;
                let picture = self.kumo.update(cx, |kumo, _| kumo.take_picture());
                self.dock.update(cx, |dock, cx| {
                    dock.show_pull(picture, drawn);
                    cx.notify();
                });
            }
            Commit::Waiting => {}
            Commit::Full(error) => {
                tracing::debug!(%error, "can't own the pulled Kumo; the dock keeps its picture");
            }
            Commit::Failed(error) => {
                tracing::debug!(%error, "can't show the pulled Kumo; the dock keeps its picture");
            }
        }
    }

    /// The line holds the dock again, so a pull still going regrows the overlay at its next
    /// frame. A resting pull is over: the dock draws no picture and the peek shows the body.
    pub(super) fn line_reclaimed(&mut self, cx: &mut App) {
        self.pull_dock.area = None;
        self.pull_dock.reclaiming = false;
        if self.kumo.update(cx, KumoView::rested) {
            self.pull_dock.shown.clear(0, 0);
            self.dock.update(cx, |dock, cx| {
                dock.show_pull(None, dock.drawn);
                cx.notify();
            });
        }
    }
}

impl super::line::Dock {
    /// Shows `picture`, or none. The images it no longer draws leave GPUI's atlas a frame
    /// later; the body's, drawn by every picture of a pull, stays.
    pub(super) fn show_pull(&mut self, picture: Option<PullPicture>, drawn: Drawn) {
        self.undrawn = picture.is_some();
        let old = std::mem::replace(&mut self.pull, picture);
        let kept = |image: &Arc<RenderImage>| {
            self.pull
                .iter()
                .flat_map(|picture| picture.images.iter().flatten())
                .any(|(_, kept)| Arc::ptr_eq(kept, image))
        };
        for (_, image) in old.into_iter().flat_map(|old| old.images).flatten() {
            if !kept(&image) {
                self.retired.push(image);
            }
        }
        self.drawn = drawn;
    }

    /// Whether the dock has drawn the pulled picture it holds.
    pub(super) fn drew_pull(&self) -> bool {
        self.pull.is_some() && !self.undrawn
    }
}

#[cfg(test)]
mod tests {
    use super::{Commit, Drawing, PullDock};
    use crate::presence::line::LineFrame;
    use crate::presence::pull_picture::Rows;

    const AREA: [i32; 4] = [0, 0, 200, 40];

    /// A picture owning `[left, right)` on rows 30 and 31, below the line's band.
    fn picture(left: i32, right: i32) -> Rows {
        let mut rows = Rows::with_capacity(40);
        rows.clear(0, 40);
        for y in [30, 31] {
            rows.push(y, [left, right]).expect("room");
        }
        rows
    }

    /// Commits `next`, returning what happened and the mask Windows was given, if any.
    fn commit(
        dock: &mut PullDock,
        drawing: Drawing,
        next: &mut Rows,
        upload: Result<(), &'static str>,
    ) -> (Commit<&'static str>, Option<Rows>) {
        let line = LineFrame::in_slot([0, 0, 58, 21], 1.0);
        let mut given = None;
        let commit = dock.commit(drawing, &line, AREA, next, |_, mask| {
            given = Some(mask.clone());
            upload
        });
        (commit, given)
    }

    #[test]
    fn a_picture_accepted_but_not_drawn_holds_back_the_next_until_it_is() {
        let mut dock = PullDock::new(40);
        let (mut p1, mut p2, mut p3) = (picture(100, 110), picture(140, 150), picture(170, 180));
        assert_eq!(
            commit(&mut dock, Drawing::Drawn, &mut p1, Ok(())).0,
            Commit::Accepted
        );
        assert_eq!(
            commit(&mut dock, Drawing::Pending, &mut p2, Ok(())),
            (Commit::Waiting, None),
            "P1 may not be on screen yet: no mask leaves it unowned"
        );
        let (refused, given) = commit(&mut dock, Drawing::Drawn, &mut p2, Err("refused"));
        assert_eq!(refused, Commit::Failed("refused"));
        let given = given.expect("tried");
        assert_eq!(
            given.row(30),
            [Some([100, 110]), Some([140, 150]), None, None],
            "P2 was offered beside the shown P1"
        );
        let (accepted, given) = commit(&mut dock, Drawing::Drawn, &mut p3, Ok(()));
        assert_eq!(accepted, Commit::Accepted);
        let given = given.expect("uploaded");
        assert_eq!(
            given.row(30),
            [Some([100, 110]), Some([170, 180]), None, None],
            "P1, still drawn after P2 was refused, stays owned beside P3"
        );
        assert_eq!(
            given.row(5),
            [Some([1, 57]), None, None, None],
            "and the line's band"
        );
    }

    #[test]
    fn a_mask_with_a_fifth_span_in_a_row_is_never_uploaded() {
        let mut dock = PullDock::new(40);
        let mut wide = Rows::with_capacity(40);
        wide.clear(0, 40);
        for left in [100, 120, 140, 160] {
            wide.push(30, [left, left + 5]).expect("room for four");
        }
        assert_eq!(
            commit(&mut dock, Drawing::Drawn, &mut wide, Ok(())).0,
            Commit::Accepted
        );
        let mut next = picture(180, 190);
        let (full, given) = commit(&mut dock, Drawing::Drawn, &mut next, Ok(()));
        assert!(matches!(full, Commit::Full(_)));
        assert_eq!(given, None);
    }
}
