//! The dock at rest: a short line near the work area's edge. GPUI draws it as an antialiased
//! rounded rect, and the hit proxy owns it plus a trigger band above and below, so the pointer
//! finds it without the line growing. A placement becomes current only once Windows accepts its
//! mask, so the line never shows where the mask isn't.

pub(crate) use kumochi_shell_core::presence::line::machine;

use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Context, IntoElement, ObjectFit, ParentElement as _, Render, RenderImage, SharedString,
    Styled as _, StyledImage as _, WeakEntity, Window, canvas, div, img, px,
};

use super::diagnostics::{self, PendingPaint};
use super::kumo_view::PullSite;
use super::observer::{self, Drawn, Surface};
use super::paint::Stamp;
use super::pull_picture::PullPicture;
use super::{OpenError, Presence, platform};
use crate::platform::{PresenceError, PresenceNative};
use machine::{Attempt, ROW_SPANS, Surfaces};
pub(super) use machine::{Line, LineFrame, Placement, slot};

/// The widest the drop label gets, in logical pixels; a longer title is cut short.
const LABEL_WIDTH: f32 = 320.0;

/// Runs `step` on `owner` at the window's next frame, with that frame's own window: GPUI
/// holds the window while its frame callbacks run, so a handle can't reach it then.
pub(super) fn on_next_frame<T: 'static>(
    window: &Window,
    owner: WeakEntity<T>,
    step: fn(&mut T, &mut Window, &mut Context<'_, T>),
) {
    window.on_next_frame(move |window, cx| {
        if owner.update(cx, |this, cx| step(this, window, cx)).is_err() {
            tracing::debug!("the presence closed before its next frame");
        }
    });
}

/// The real surfaces: the proxy's mask and visibility, and the dock's overlay window.
struct Desktop<'native, 'ui> {
    native: &'native mut PresenceNative,
    window: &'ui mut Window,
    cx: &'ui mut App,
    /// The observer's state after the last accepted upload, which the dock names when it draws.
    drawn: Drawn,
}

impl Surfaces for Desktop<'_, '_> {
    type Error = PresenceError;

    fn upload(&mut self, frame: &LineFrame) -> Result<(), PresenceError> {
        let begin_qpc = observer::begin(self.cx);
        let update = self.native.reshape(frame.hit, |y| frame.row_spans(y));
        let drawn = observer::commit(self.cx, begin_qpc, Some(frame), frame.hit, &update);
        update?;
        self.drawn = drawn;
        Ok(())
    }

    fn move_overlay(&mut self, overlay: [i32; 4]) -> Result<(), PresenceError> {
        platform::place_presence_window(self.window, overlay)?;
        self.window.bounds_changed(self.cx);
        Ok(())
    }

    fn show(&mut self, index: usize) -> Result<(), PresenceError> {
        self.native.show(index)
    }

    fn hide_overlay(&mut self) -> Result<(), PresenceError> {
        platform::hide_presence_window(self.window)
    }

    fn hide_proxy(&mut self) -> Result<(), PresenceError> {
        self.native.hide()
    }

    fn clear_mask(&mut self, hit: [i32; 4]) -> Result<(), PresenceError> {
        let begin_qpc = observer::begin(self.cx);
        let update = self.native.reshape(hit, |_| [None; ROW_SPANS]);
        let drawn = observer::commit(self.cx, begin_qpc, None, hit, &update);
        update?;
        self.drawn = drawn;
        Ok(())
    }
}

impl Presence {
    /// Asks for the dock at `placement` and tries it now; if it isn't current yet, the dock
    /// stays as it was and the next frames retry.
    ///
    /// # Errors
    /// Fails when the dock window is gone.
    pub(super) fn request_line(
        &mut self,
        placement: Placement,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.line.request(placement);
        let owner = cx.weak_entity();
        let handle = self.dock_window;
        handle
            .update(cx, |_, window, cx| self.try_line(owner, window, cx))
            .map_err(|error| OpenError::Gpui(error.into()))?;
        Ok(())
    }

    fn retry_line(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.try_line(cx.weak_entity(), window, cx)
            && let Err(error) = self.pointer_changed(cx)
        {
            tracing::debug!(%error, "can't recheck hover after the dock moved");
        }
    }

    /// One attempt at the requested placement. Returns whether it became current, with its
    /// layout and line published.
    pub(super) fn try_line(
        &mut self,
        owner: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let mut surfaces = Desktop {
            native: &mut self.native,
            window,
            cx,
            drawn: self.drawn,
        };
        let attempt = self.line.attempt(&mut surfaces);
        let Desktop {
            window, cx, drawn, ..
        } = surfaces;
        self.drawn = drawn;
        match attempt {
            Attempt::Idle => false,
            Attempt::Placed(placement) => {
                self.layout = placement.layout;
                if let Err(error) = self.reanchor_panels(cx) {
                    tracing::warn!(%error, "can't re-anchor open panels after the dock moved");
                }
                self.place_drawings(owner.clone(), cx);
                let origin = [placement.layout.overlay[0], placement.layout.overlay[1]];
                self.dock.update(cx, |dock, cx| {
                    dock.paint = Some(placement.frame.paint);
                    dock.origin = origin;
                    dock.drawn = drawn;
                    cx.notify();
                });
                let site = PullSite {
                    line: placement.frame.paint,
                    slot: placement.layout.dock,
                };
                self.kumo.update(cx, |kumo, _| kumo.set_site(Some(site)));
                self.line_reclaimed(cx);
                true
            }
            Attempt::Failed {
                error,
                first,
                retry,
                closed,
            } => {
                if first {
                    tracing::warn!(%error, "can't place the dock");
                }
                if closed {
                    tracing::error!(%error, "the dock is hidden until the next position choice");
                    self.dock.update(cx, |dock, cx| {
                        dock.paint = None;
                        cx.notify();
                    });
                    self.kumo.update(cx, |kumo, _| kumo.set_site(None));
                    self.line_reclaimed(cx);
                }
                if retry {
                    on_next_frame(window, owner, Self::retry_line);
                }
                false
            }
            Attempt::Stranded(error) => {
                tracing::error!(%error, "the dock's band may still take clicks, so the shell quits");
                cx.defer(move |cx| {
                    if owner.update(cx, Self::quit).is_err() {
                        tracing::debug!("the presence closed before it could quit");
                    }
                });
                false
            }
        }
    }
}

/// The dock window's view: the accepted line, drawn where Windows accepted its mask.
pub(super) struct Dock {
    /// Used only with `--input-diagnostics`; the next render takes it into a paint-phase marker.
    pub(super) pending_paint: PendingPaint,
    /// The accepted line's device pixels, or none before the first accepted upload.
    pub(super) paint: Option<[i32; 4]>,
    /// The dock window's top-left on screen, in physical pixels.
    pub(super) origin: [i32; 2],
    /// The accepted mask state `paint` came from, for `--gate-observer`.
    pub(super) drawn: Drawn,
    /// While Listening, the line takes the accent colour.
    pub(super) listening: bool,
    /// The pulled picture whose mask Windows accepted, drawn over the line.
    pub(super) pull: Option<PullPicture>,
    /// Images no picture draws any more, and those retired a frame earlier, which leave
    /// GPUI's atlas at this render.
    pub(super) retired: Vec<Arc<RenderImage>>,
    pub(super) aged: Vec<Arc<RenderImage>>,
    /// `pull` changed, to an accepted picture or to none as a pull lets go, and hasn't been
    /// rendered since.
    pub(super) undrawn: bool,
    /// The drop label under a carried body: screen text, drawn and never logged.
    pub(super) label: Option<SharedString>,
}

impl Dock {
    pub(super) fn new() -> Self {
        Self {
            pending_paint: PendingPaint::default(),
            paint: None,
            origin: [0; 2],
            drawn: Drawn::default(),
            listening: false,
            pull: None,
            // Clearing a pull's picture retires all it draws: drip, stub and body.
            retired: Vec::with_capacity(3),
            aged: Vec::with_capacity(3),
            undrawn: false,
            label: None,
        }
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Recorded in GPUI's paint phase, not at present: PresentMon must join it to the screen.
        let marker = self.pending_paint.take().map(|at| {
            canvas(
                |_, _, _| (),
                move |_, (), _, cx| {
                    let painted_us = at.elapsed().as_micros();
                    diagnostics::record(cx, format_args!("listening painted_us={painted_us}"));
                },
            )
            .absolute()
            .size_full()
        });
        for image in self.aged.drain(..) {
            if let Err(error) = window.drop_image(image) {
                tracing::debug!(%error, "can't drop a pulled frame");
            }
        }
        std::mem::swap(&mut self.aged, &mut self.retired);
        self.undrawn = false;
        let scale = window.scale_factor();
        let fill = if self.listening {
            cx.theme().primary
        } else {
            cx.theme().background
        };
        let [origin_x, origin_y] = self.origin;
        let line = self.paint.map(|[x, y, width, height]| {
            div()
                .absolute()
                .left(logical(x - origin_x, scale))
                .top(logical(y - origin_y, scale))
                .w(logical(width, scale))
                .h(logical(height, scale))
                .rounded_full()
                .bg(fill)
                .border_1()
                .border_color(cx.theme().border)
        });
        let pulled = self
            .pull
            .iter()
            .flat_map(|picture| picture.images.iter().flatten());
        let pulled = pulled.map(|(quad, image)| {
            img(Arc::clone(image))
                .absolute()
                .left(logical(quad.x - origin_x, scale))
                .top(logical(quad.y - origin_y, scale))
                .w(logical(quad.width, scale))
                .h(logical(quad.height, scale))
                .object_fit(ObjectFit::Fill)
        });
        let body = self
            .pull
            .as_ref()
            .and_then(|picture| picture.images[2].as_ref());
        let label = self.label.clone().zip(body).map(|(text, (quad, _))| {
            div()
                .absolute()
                .left(logical(quad.x + quad.width / 2 - origin_x, scale) - px(LABEL_WIDTH / 2.0))
                .top(logical(quad.y + quad.height - origin_y, scale) + px(4.0))
                .w(px(LABEL_WIDTH))
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_2()
                        .rounded_md()
                        .bg(cx.theme().popover.opacity(0.85))
                        .text_color(cx.theme().popover_foreground)
                        .text_sm()
                        .truncate()
                        .child(text),
                )
        });
        div()
            .size_full()
            .relative()
            .child(Stamp(Surface::Dock, self.drawn))
            .children(line)
            .children(pulled)
            .children(label)
            // After the frame's stamp, so the paint qpc it logs falls inside that frame.
            .children(marker)
    }
}

/// Physical pixels as GPUI's logical pixels.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "screen coordinates are far below f32's exact integer range"
)]
fn logical(physical: i32, scale: f32) -> gpui_kit::Pixels {
    px(physical as f32 / scale)
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, Context, IntoElement, Render, Window, WindowOptions, div};

    use super::machine::fixture::{Fake, at_a, failed, placement};
    use super::{Attempt, Line, on_next_frame};
    use crate::presence::pull_dock::{Holding, PullDock, Rest};

    #[test]
    fn a_pull_keeps_its_mask_until_the_dock_draws_no_body_then_a_refused_shrink_keeps_a_band() {
        let (a, b) = (placement(0, 0), placement(400, 2));
        let (mut line, mut fake) = at_a();
        let area = [-228, -228, 514, 477];
        line.pull_overlay();
        (fake.mask, fake.pulled) = (None, Some(area));
        line.pull_mask(area);
        line.request(b);
        assert_eq!(
            line.attempt(&mut fake),
            Attempt::Idle,
            "the dock draws the body"
        );
        assert_eq!(
            (fake.mask, fake.pulled),
            (None, Some(area)),
            "the body's pixels stay owned"
        );
        let mut dock = PullDock::new(477);
        assert_eq!(dock.rest(Holding::Picture), Rest::Release);
        assert_eq!(dock.rest(Holding::Clearing), Rest::Wait);
        assert_eq!(dock.rest(Holding::Nothing), Rest::Reclaim);
        line.reclaim();
        fake.refuse_moves = vec![true];
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert_eq!(
            fake.mask,
            Some(a.frame),
            "the band under the line the dock still draws"
        );
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(b));
        assert!(fake.holds(b));
    }

    /// The dock's retry on a fake desktop, driven by real frame callbacks.
    struct Probe {
        line: Line,
        fake: Fake,
        attempts: usize,
    }

    impl Probe {
        fn step(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
            self.attempts += 1;
            if let Attempt::Failed { retry: true, .. } = self.line.attempt(&mut self.fake) {
                on_next_frame(window, cx.weak_entity(), Self::step);
            }
        }
    }

    /// The window's root view; the probe is a separate entity, as the presence is to the dock.
    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
            div()
        }
    }

    /// Requests B with uploads refused per `refuse`, runs frames until none is pending, and
    /// returns the attempts, whether Windows holds B, and how many frames ran a retry.
    fn retry(cx: &mut TestAppContext, refuse: Vec<bool>) -> (usize, bool, usize) {
        let probe = cx.new(|_| Probe {
            line: Line::default(),
            fake: Fake {
                refuse_uploads: refuse,
                ..Fake::default()
            },
            attempts: 0,
        });
        let window = cx
            .update(|cx| cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Blank)))
            .expect("a window");
        window
            .update(cx, |_, window, cx| {
                probe.update(cx, |probe, cx| {
                    probe.line.request(placement(400, 2));
                    probe.step(window, cx);
                });
            })
            .expect("the window");
        let mut frames = 0;
        while window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .expect("the window")
            > 0
        {
            frames += 1;
        }
        probe.read_with(cx, |probe, _| {
            (probe.attempts, probe.fake.holds(placement(400, 2)), frames)
        })
    }

    #[gpui::test]
    fn two_failures_then_a_success_place_the_dock_through_frame_callbacks(cx: &mut TestAppContext) {
        assert_eq!(retry(cx, vec![true, true]), (3, true, 2));
    }

    #[gpui::test]
    fn three_failures_stop_retrying(cx: &mut TestAppContext) {
        assert_eq!(retry(cx, vec![true, true, true]), (3, false, 2));
    }
}
