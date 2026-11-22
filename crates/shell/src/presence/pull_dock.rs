//! The dock's end of the pulled picture. The dock's window, shown through the whole pull and
//! its return, paces the pull's frames, since the peek may hide once the drag ends. Each
//! frame the overlay covers the pull area, the mask owns the line, the next picture and the
//! one the dock shows, and the dock draws the next picture only once Windows accepts that
//! mask, one picture per dock draw. Once the body rests, the line takes the dock back. How it
//! fits: docs/development/kumo.md, "The pulled picture".

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::{App, Context, RenderImage, Window};

use super::kumo_view::KumoView;
use super::line::on_next_frame;
use super::observer::{self, Drawn};
use super::pull_picture::PullPicture;
use super::{Presence, platform};

pub(super) use kumochi_shell_core::presence::pull_dock::machine;

pub(super) use machine::{Commit, Drawing, Holding, PullDock, Rest};

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
            self.follow_carry(cx);
        }
        if paced.is_none() || self.kumo.read(cx).pull_wants_frame() || self.dock.read(cx).undrawn {
            on_next_frame(window, cx.weak_entity(), Self::pull_frame);
        } else {
            self.pull_dock.frames.idle();
        }
    }

    /// Commits the pull's next picture, or once the body rests, hands it back to the peek and
    /// then asks the line to take the dock back; the pull ends only once that's accepted.
    fn show_pull(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let dock = self.dock.read(cx);
        let (holding, drawing) = match (dock.pull.is_some(), dock.undrawn) {
            (true, false) => (Holding::Picture, Drawing::Drawn),
            (true, true) => (Holding::Picture, Drawing::Pending),
            (false, true) => (Holding::Clearing, Drawing::Pending),
            (false, false) => (Holding::Nothing, Drawing::Drawn),
        };
        if self.kumo.read(cx).resting() {
            match self.pull_dock.rest(holding) {
                Rest::Release => {
                    self.kumo.update(cx, KumoView::dock_released);
                    self.dock.update(cx, |dock, cx| {
                        dock.let_go();
                        cx.notify();
                    });
                }
                Rest::Wait => {}
                Rest::Reclaim => {
                    self.line.reclaim();
                    self.try_line(cx.weak_entity(), window, cx);
                }
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

    /// The line holds the dock again; while a pull holds it the line takes no attempt, so a
    /// pull that was resting is over now and the dock owns none of its pixels.
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

    /// Stops drawing the pulled picture; `undrawn` until a frame without it is rendered.
    pub(super) fn let_go(&mut self) {
        self.show_pull(None, self.drawn);
        self.undrawn = true;
    }

    /// Whether the dock has drawn the pulled picture it holds.
    pub(super) fn drew_pull(&self) -> bool {
        self.pull.is_some() && !self.undrawn
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::RenderImage;

    use crate::presence::kumo::Quad;
    use crate::presence::line::Dock;
    use crate::presence::observer::Drawn;
    use crate::presence::pull_picture::PullPicture;

    const AREA: [i32; 4] = [0, 0, 200, 40];

    #[test]
    fn letting_go_of_a_drip_stub_and_body_retires_all_three_without_growing() {
        let image = || {
            let pixels = image::RgbaImage::new(1, 1);
            Some((
                Quad {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                Arc::new(RenderImage::new([image::Frame::new(pixels)])),
            ))
        };
        let mut dock = Dock::new();
        let reserved = dock.retired.capacity();
        let picture = PullPicture {
            area: AREA,
            images: [image(), image(), image()],
        };
        dock.show_pull(Some(picture), Drawn::default());
        dock.let_go();
        assert_eq!(dock.retired.len(), 3, "the drip, the stub and the body");
        assert_eq!(
            dock.retired.capacity(),
            reserved,
            "made at the dock's start"
        );
        assert!(
            dock.pull.is_none() && dock.undrawn,
            "a frame without them is due"
        );
    }
}
