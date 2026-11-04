//! Kumo in the peek: a retained view that bakes the body off the UI thread, shades frames from
//! the animator while something engages it, and leaves its slot empty until the first bake
//! lands. Nothing here runs at rest: a settled peek shades nothing and allocates nothing.
//! Pulling Kumo out of the peek starts here too, in `pull`.

mod picture;
mod pull;

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    App, Context, EventEmitter, IntoElement, ParentElement as _, Render, RenderImage, Styled as _,
    Task, Window, canvas, div, img, px,
};

use super::kumo::{Animator, CARRY_WIDTH, Look, Mood, Motion, Renderer, Silhouette};
use super::line::{on_next_frame, slot};
use super::pull_picture::{PullArt, PullPicture, Rows, largest_area};
use super::pulling::Pulling;
use super::{Presence, diagnostics};
pub(super) use pull::{PullChanged, PullSite, bridge};

/// Kumo's size in the peek, in logical pixels.
pub(super) const SIZE: f32 = 64.0;

/// The view wants animation frames: its owner arms the frame loop.
pub(super) struct WantsFrames;

pub(super) struct KumoView {
    /// The bake wanted: `SIZE` at the window's scale, in device pixels.
    size: u16,
    /// The accepted bake, kept across hide and reopen.
    renderer: Option<Arc<Renderer>>,
    bake: Option<Task<()>>,
    animator: Animator,
    image: Option<Arc<RenderImage>>,
    /// The image the last frame replaced, dropped from GPUI's atlas a frame later.
    retired: Option<Arc<RenderImage>>,
    shown: bool,
    /// The last shaded frame, to count reshades that changed nothing.
    last: Option<super::kumo::FrameState>,
    reshades: u64,
    unchanged: u64,
    /// Time spent shading and wrapping frames, for the Quit totals.
    shading: Duration,
    pulling: Pulling,
    /// Where a grab starts, from the line Windows last accepted.
    site: Option<PullSite>,
    /// The peek's client origin at the grab, physical screen pixels.
    origin: [i32; 2],
    /// The shell is quitting: no new grab starts.
    closing: bool,
    /// What a pull draws, from the carry-width bake.
    art: Option<PullArt>,
    /// The next pulled picture, waiting for the dock to accept its mask, and its pixels.
    next: Option<PullPicture>,
    next_rows: Rows,
    /// Which window shows the body.
    pulled: Pulled,
    /// The released body is home and still: the dock's next accepted mask is the line alone,
    /// and the pull wants no frame until then. Whichever window showed the body still does.
    resting: bool,
}

/// Which window shows the body: the peek until the dock has drawn a pulled picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pulled {
    Peek,
    /// The dock has drawn the pulled body, so the peek's slot is empty.
    Dock,
}

impl EventEmitter<WantsFrames> for KumoView {}

impl KumoView {
    pub(super) fn new() -> Self {
        Self {
            size: 0,
            renderer: None,
            bake: None,
            animator: Animator::new(Mood::Idle, 1),
            image: None,
            retired: None,
            shown: false,
            last: None,
            reshades: 0,
            unchanged: 0,
            shading: Duration::ZERO,
            pulling: Pulling::default(),
            site: None,
            origin: [0, 0],
            closing: false,
            art: None,
            next: None,
            next_rows: Rows::default(),
            pulled: Pulled::Peek,
            resting: false,
        }
    }

    /// Bakes the body for `scale` on the background executor, unless that size is baked or
    /// baking. Only a bake still wanted when it lands is kept; one that lands while the peek is
    /// hidden is kept too, as a prewarm.
    // shortcut: only the launch scale is baked, so a scale change needs a restart. A second
    // caller (OSC-188) must fix two hazards first: `self.size` changes before the matching bake
    // lands, so a step wraps the old renderer's pixels at the new size, and a landing bake can
    // overwrite a pending `retired` image without `drop_image`.
    pub(super) fn rebake(&mut self, scale: f32, cx: &mut Context<'_, Self>) {
        let size = device(SIZE, scale);
        if size == self.size {
            return;
        }
        self.size = size;
        self.bake = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let (renderer, art) = cx
                .background_executor()
                .spawn(async move {
                    let carry = device(CARRY_WIDTH, scale);
                    let art = PullArt::new(
                        &Renderer::new(&Silhouette::emoji(), carry, Look::default()),
                        carry,
                        Silhouette::emoji(),
                        Look::default(),
                        pull_rows(scale),
                    );
                    (
                        Renderer::new(&Silhouette::emoji(), size, Look::default()),
                        art,
                    )
                })
                .await;
            let baked_us = started.elapsed().as_micros();
            let kept = this.update(cx, |view, cx| {
                if view.size != size {
                    return;
                }
                view.bake = None;
                view.renderer = Some(Arc::new(renderer));
                view.art = art
                    .inspect_err(|error| tracing::error!(%error, "can't bake the pulled Kumo"))
                    .ok();
                view.next_rows = Rows::with_capacity(pull_rows(scale));
                // An image from another size would show at the wrong scale.
                view.retired = view.image.take().or(view.retired.take());
                diagnostics::record(cx, format_args!("kumo baked size={size} us={baked_us}"));
                if view.shown {
                    view.animator.engage();
                    cx.emit(WantsFrames);
                }
            });
            if kept.is_err() {
                tracing::debug!("the peek closed before Kumo's bake landed");
            }
        }));
    }

    /// The peek opened: one blink, or a still pose under reduced motion. Before the first bake
    /// the blink waits for it.
    pub(super) fn show(&mut self, motion: Motion, cx: &mut Context<'_, Self>) {
        self.shown = true;
        self.animator.set_motion(motion);
        if self.renderer.is_some() {
            self.animator.engage();
            cx.emit(WantsFrames);
        }
    }

    /// The peek closed: a blink under way is cut, a late bake won't blink, and a live drag is
    /// lost with the window holding it.
    pub(super) fn hide(&mut self, cx: &mut Context<'_, Self>) {
        self.shown = false;
        self.animator.disengage();
        self.pull_event(super::drag::Event::SourceClosed, cx);
    }

    /// Listening began or ended, under the motion policy read at that edge.
    pub(super) fn set_mood(&mut self, mood: Mood, motion: Motion, cx: &mut Context<'_, Self>) {
        self.animator.set_motion(motion);
        self.animator.set_mood(mood);
        if self.wants_frame() {
            cx.emit(WantsFrames);
        }
    }

    /// Whether the next frame has something to draw or to drop. A pull's frames come from the
    /// dock, which stays shown through its return.
    pub(super) fn wants_frame(&self) -> bool {
        self.retired.is_some()
            || (self.shown
                && self.renderer.is_some()
                && (self.image.is_none() || self.animator.is_animating()))
    }

    /// One paced frame: drops the image the last one replaced, then shades the next if
    /// there's one to show. Returns whether another frame is wanted.
    pub(super) fn step(
        &mut self,
        dt: f64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "a frame step is at most two intervals"
        )]
        let dt = dt as f32;
        if let Some(retired) = self.retired.take()
            && let Err(error) = window.drop_image(retired)
        {
            tracing::debug!(%error, "can't drop Kumo's last frame");
        }
        if let Some(renderer) = self.renderer.clone()
            && self.shown
            && (self.image.is_none() || self.animator.is_animating())
        {
            let frame = self.animator.tick(dt, 0.0);
            self.reshades += 1;
            self.unchanged += u64::from(self.last == Some(frame));
            self.last = Some(frame);
            let started = Instant::now();
            // shortcut: two allocations per animated frame, the pixels and the image's Arc
            // (OSC-175's exception); pool the images if a profile ever shows them.
            let mut pixels = vec![0; renderer.image_len()];
            match renderer.render(&frame, &mut pixels) {
                Ok(()) => {
                    let side = u32::from(self.size);
                    if let Some(buffer) = image::RgbaImage::from_raw(side, side, pixels) {
                        let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
                        self.retired = self.image.replace(image);
                        cx.notify();
                    }
                }
                Err(error) => tracing::error!(%error, "can't shade Kumo"),
            }
            let shaded = started.elapsed();
            self.shading += shaded;
            diagnostics::record(
                cx,
                format_args!("kumo frame shade_us={}", shaded.as_micros()),
            );
        }
        self.wants_frame()
    }

    /// At Quit: how many frames were shaded, how many repeated the one before, and the
    /// time spent shading them.
    pub(super) fn totals(&self) -> ([u64; 2], Duration) {
        ([self.reshades, self.unchanged], self.shading)
    }
}

impl Render for KumoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        div()
            .relative()
            .size(px(SIZE))
            .flex_none()
            .children(
                self.image
                    .clone()
                    .filter(|_| self.pulled == Pulled::Peek)
                    .map(|image| img(image).size_full()),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| {
                        pull::listen(bounds, view, window);
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl Presence {
    /// Arms the frame loop on the peek's window, unless it's armed already.
    pub(super) fn want_frames(&mut self, cx: &mut Context<'_, Self>) {
        if !self.frames.want(Instant::now()) {
            return;
        }
        let owner = cx.weak_entity();
        let armed = self
            .panel_window
            .update(cx, |_, window, _| on_next_frame(window, owner, Self::frame));
        if let Err(error) = armed {
            self.frames.idle();
            tracing::warn!(%error, "can't animate Kumo");
        }
    }

    /// One frame callback: the pacer first, so a skipped frame does no animation work.
    fn frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let cap = self
            .settings
            .read(cx)
            .view()
            .map(|view| view.max_frame_rate)
            .unwrap_or_default();
        let wanted = match self
            .frames
            .frame(Instant::now(), cap, window.refresh_interval(cx))
        {
            None => true,
            Some(dt) => self.kumo.update(cx, |kumo, cx| kumo.step(dt, window, cx)),
        };
        if wanted {
            on_next_frame(window, cx.weak_entity(), Self::frame);
        } else {
            self.frames.idle();
        }
    }
}

/// Kumo's motion policy now. Windows posts no change Kit can follow, so each edge that starts
/// motion re-reads the system preference.
pub(super) fn motion(cx: &mut App) -> Motion {
    gpui_kit::base::apply_system_reduce_motion(cx);
    if cx.reduce_motion() {
        Motion::Reduced
    } else {
        Motion::Full
    }
}

/// The tallest pull area at `scale`, in rows.
pub(super) fn pull_rows(scale: f32) -> usize {
    usize::try_from(largest_area(slot(scale), scale)[1]).unwrap_or(0)
}

/// `logical` pixels in device pixels at `scale`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Kumo's side at any real scale fits u16"
)]
fn device(logical: f32, scale: f32) -> u16 {
    (logical * scale).round().clamp(1.0, f32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::TestAppContext;
    use gpui_kit::{
        AppContext as _, Context, Entity, IntoElement, Render, Window, WindowOptions, div,
    };

    use super::{KumoView, Mood, Motion};

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
            div()
        }
    }

    /// A view baked at `scale`, with a count of the frames it asked for.
    fn baked(cx: &mut TestAppContext, scale: f32) -> (Entity<KumoView>, Rc<Cell<u32>>) {
        let view = cx.new(|_| KumoView::new());
        let asked = Rc::new(Cell::new(0));
        let counter = Rc::clone(&asked);
        cx.update(|cx| {
            cx.subscribe(&view, move |_, _: &super::WantsFrames, _| {
                counter.set(counter.get() + 1);
            })
            .detach();
        });
        view.update(cx, |view, cx| view.rebake(scale, cx));
        cx.run_until_parked();
        (view, asked)
    }

    /// Steps at 72 fps until the view wants no more frames, or `limit` frames; returns how many
    /// it stepped.
    fn step_up_to(cx: &mut TestAppContext, view: &Entity<KumoView>, limit: usize) -> usize {
        let window = cx
            .update(|cx| cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Blank)))
            .expect("a window");
        let mut frames = 0;
        while frames < limit
            && window
                .update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| view.step(1.0 / 72.0, window, cx))
                })
                .expect("the window")
        {
            frames += 1;
        }
        frames
    }

    fn settle(cx: &mut TestAppContext, view: &Entity<KumoView>) -> usize {
        let frames = step_up_to(cx, view, 200);
        assert!(frames < 200, "the view never settled");
        frames
    }

    fn shaded(cx: &mut TestAppContext, view: &Entity<KumoView>) -> u64 {
        view.read_with(cx, |view, _| view.totals().0[0])
    }

    /// A full-motion blink at 72 fps: its 0.17 s close and open end on the 13th frame.
    const BLINK: usize = 13;

    #[gpui::test]
    fn a_hover_blinks_once_then_shades_nothing_more(cx: &mut TestAppContext) {
        let (view, asked) = baked(cx, 1.0);
        assert_eq!(asked.get(), 0, "a hidden view asks for no frames");
        view.update(cx, |view, cx| view.show(Motion::Full, cx));
        assert_eq!(asked.get(), 1);
        assert_eq!(settle(cx, &view), BLINK);
        let ([reshades, unchanged], _) = view.read_with(cx, |view, _| view.totals());
        assert_eq!(unchanged, 0, "no frame repeated the one before");
        assert_eq!(settle(cx, &view), 0, "a settled peek wants no frames");
        assert_eq!(shaded(cx, &view), reshades);
        assert!(view.read_with(cx, |view, _| view.image.is_some()));
    }

    #[gpui::test]
    fn reduced_motion_shades_one_still_frame(cx: &mut TestAppContext) {
        let (view, _) = baked(cx, 1.0);
        view.update(cx, |view, cx| view.show(Motion::Reduced, cx));
        settle(cx, &view);
        assert_eq!(view.read_with(cx, |view, _| view.totals()).0, [1, 0]);
    }

    #[gpui::test]
    fn a_show_before_the_bake_blinks_when_its_wake_up_lands(cx: &mut TestAppContext) {
        let view = cx.new(|_| KumoView::new());
        let asked = Rc::new(Cell::new(0));
        let counter = Rc::clone(&asked);
        cx.update(|cx| {
            cx.subscribe(&view, move |_, _: &super::WantsFrames, _| {
                counter.set(counter.get() + 1);
            })
            .detach();
        });
        view.update(cx, |view, cx| {
            view.rebake(1.0, cx);
            view.show(Motion::Full, cx);
            assert!(!view.wants_frame(), "nothing to shade before the bake");
        });
        assert_eq!(asked.get(), 0);
        cx.run_until_parked();
        assert_eq!(asked.get(), 1, "the landing bake asks for frames");
        assert_eq!(settle(cx, &view), BLINK);
    }

    #[gpui::test]
    fn only_the_bake_for_the_current_scale_is_kept(cx: &mut TestAppContext) {
        let view = cx.new(|_| KumoView::new());
        view.update(cx, |view, cx| {
            view.rebake(1.0, cx);
            view.rebake(2.0, cx);
        });
        cx.run_until_parked();
        let len = view.read_with(cx, |view, _| {
            view.renderer.as_ref().map(|renderer| renderer.image_len())
        });
        assert_eq!(len, Some(128 * 128 * 4));
    }

    #[gpui::test]
    fn hiding_mid_blink_shades_nothing_more(cx: &mut TestAppContext) {
        let (view, _) = baked(cx, 1.0);
        view.update(cx, |view, cx| view.show(Motion::Full, cx));
        assert_eq!(step_up_to(cx, &view, 3), 3);
        view.update(cx, KumoView::hide);
        settle(cx, &view);
        assert_eq!(shaded(cx, &view), 3);
        assert!(!view.read_with(cx, |view, _| view.wants_frame()));
    }

    #[gpui::test]
    fn a_preference_changed_after_the_first_opening_holds_at_the_next_edges(
        cx: &mut TestAppContext,
    ) {
        let (view, _) = baked(cx, 1.0);
        view.update(cx, |view, cx| view.show(Motion::Full, cx));
        assert_eq!(settle(cx, &view), BLINK);
        view.update(cx, KumoView::hide);
        settle(cx, &view);
        cx.update(|cx| cx.set_reduce_motion(true));
        let motion = cx.update(super::motion);
        assert_eq!(motion, Motion::Reduced);
        let opened = shaded(cx, &view);
        view.update(cx, |view, cx| view.show(motion, cx));
        assert_eq!(
            settle(cx, &view),
            1,
            "reopening shades one still frame, no blink"
        );
        assert_eq!(shaded(cx, &view), opened + 1);
        let before = shaded(cx, &view);
        view.update(cx, |view, cx| view.set_mood(Mood::Listening, motion, cx));
        assert_eq!(settle(cx, &view), 1, "Listening shows one still pose");
        assert_eq!(shaded(cx, &view), before + 1);
        cx.update(|cx| cx.set_reduce_motion(false));
        let motion = cx.update(super::motion);
        assert_eq!(motion, Motion::Full);
        view.update(cx, |view, cx| view.set_mood(Mood::Listening, motion, cx));
        assert_eq!(
            step_up_to(cx, &view, 10),
            10,
            "full-motion Listening keeps moving"
        );
        cx.update(|cx| cx.set_reduce_motion(true));
        let motion = cx.update(super::motion);
        view.update(cx, |view, cx| view.set_mood(Mood::Idle, motion, cx));
        assert_eq!(
            settle(cx, &view),
            1,
            "a reduced edge while open stops at once"
        );
    }
}
