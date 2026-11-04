//! The dock's end of the pulled picture: the overlay grows to the pull area, the mask owns
//! the line and every pixel the picture can show, and the dock draws a picture only once
//! Windows has accepted its mask. Once the body rests, the line takes the dock back and the
//! peek gets its body back. How it fits: docs/development/kumo.md, "The pulled picture".

use std::sync::Arc;

use gpui_kit::{App, Context, RenderImage};

use super::kumo_view::KumoView;
use super::observer::{self, Drawn};
use super::pull_picture::{PullPicture, Rows};
use super::{Presence, platform};
use crate::platform::PresenceError;

/// The dock's state during a pull: where the overlay is, and the mask's rows for the next
/// upload, kept between frames so a frame allocates none.
#[derive(Debug, Default)]
pub(super) struct PullDock {
    area: Option<[i32; 4]>,
    mask: Rows,
}

impl Presence {
    /// After Kumo's frame: the dock shows the pull's next picture once Windows accepts its
    /// mask. A failed upload keeps the picture it shows. Once the body rests, the line takes
    /// the dock back, and the pull ends only once that's accepted.
    pub(super) fn show_pull(&mut self, cx: &mut Context<'_, Self>) {
        if self.kumo.read(cx).resting() {
            self.line.reclaim();
            let owner = cx.weak_entity();
            let handle = self.dock_window;
            if let Err(error) = handle.update(cx, |_, window, cx| self.try_line(owner, window, cx))
            {
                tracing::debug!(%error, "the dock closed before the pull rested");
            }
            return;
        }
        let Some(frame) = self.line.accepted_frame() else {
            return;
        };
        let kumo = self.kumo.read(cx);
        let Some((picture, next, shown)) = kumo.next_picture() else {
            return;
        };
        let area = picture.area;
        let mask = &mut self.pull_dock.mask;
        mask.clear(area[1], usize::try_from(area[3]).unwrap_or(0));
        let [_, top, _, height] = frame.hit;
        let owned = (top..top + height)
            .try_for_each(|y| {
                frame
                    .row_spans(y)
                    .into_iter()
                    .flatten()
                    .try_for_each(|span| mask.push(y, span))
            })
            .and_then(|()| mask.extend(next))
            .and_then(|()| mask.extend(shown));
        if let Err(error) = owned {
            tracing::debug!(%error, "can't own the pulled Kumo; the dock keeps its picture");
            return;
        }
        let handle = self.dock_window;
        let uploaded = handle.update(cx, |_, window, cx| {
            if self.pull_dock.area != Some(area) {
                self.line.disturb();
                platform::place_presence_window(window, area)?;
                window.bounds_changed(cx);
                self.pull_dock.area = Some(area);
                self.dock.update(cx, |dock, cx| {
                    dock.origin = [area[0], area[1]];
                    cx.notify();
                });
            }
            self.line.disturb();
            // shortcut: the observer records a pull's commits without their geometry; the
            // measurement layer gives it the line, the body and their owned spans.
            let begin_qpc = observer::begin(cx);
            let mask = &self.pull_dock.mask;
            let update = self.native.reshape(area, |y| mask.row(y));
            let drawn = observer::commit(cx, begin_qpc, None, area, &update);
            update?;
            Ok::<_, PresenceError>(drawn)
        });
        match uploaded {
            Ok(Ok(drawn)) => {
                self.drawn = drawn;
                if let Some(picture) = self.kumo.update(cx, KumoView::shown) {
                    self.dock.update(cx, |dock, cx| {
                        dock.show_pull(Some(picture), drawn);
                        cx.notify();
                    });
                }
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "can't show the pulled Kumo; the dock keeps its picture");
            }
            Err(error) => tracing::debug!(%error, "the dock closed during a pull"),
        }
    }

    /// The line holds the dock again, so a pull still going regrows the overlay at its next
    /// frame. A resting pull is over, and the dock draws no picture.
    pub(super) fn line_reclaimed(&mut self, cx: &mut App) {
        self.pull_dock.area = None;
        if self.kumo.update(cx, KumoView::rested) {
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
}
