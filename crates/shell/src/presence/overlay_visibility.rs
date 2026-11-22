//! The overlay window's visibility, kept apart from the frames' paint eligibility. A window
//! whose show or hide may not have taken effect is never shown again, and a hidden one is
//! closed, so the next drawing can't appear with the last one's frame still on its surface.

use std::{cell::RefCell, mem};

use super::OpenError;
use crate::presenter::{Frames, Sample};

/// The native operations visibility drives: GPUI windows in production, refusable in tests.
pub(super) trait Surface {
    type Window;
    /// Opens a fresh, hidden window.
    fn open(&mut self) -> Result<Self::Window, OpenError>;
    fn show(&mut self, window: &Self::Window) -> Result<(), OpenError>;
    /// Asks the window to repaint the current frames; it doesn't say the paint presented.
    fn repaint(&mut self, window: &Self::Window);
    fn hide(&mut self, window: &Self::Window) -> Result<(), OpenError>;
    /// Closes the window; a refusal still never gives it back.
    fn close(&mut self, window: Self::Window) -> Result<(), OpenError>;
}

/// Where the overlay window is. Only `Shown` makes its paints count and holds Esc.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub(super) enum Visibility<W> {
    /// No window: none was needed yet, or the last one closed. The next show opens one.
    Closed,
    /// A test's window, opened outside the presence and not shown yet. The presence itself
    /// opens a window only to show it.
    #[cfg(test)]
    Hidden(W),
    /// Its show was refused, so it may or may not be on screen.
    Showing(W),
    Shown(W),
    /// Repainted empty, then its hide was refused. It stays click-through and empty until the
    /// next show retires it: no Esc, no paint acknowledgement and no retry.
    EmptyHideFailed(W),
}

impl<W> Visibility<W> {
    pub(super) fn window(&self) -> Option<&W> {
        match self {
            Self::Closed => None,
            #[cfg(test)]
            Self::Hidden(window) => Some(window),
            Self::Showing(window) | Self::Shown(window) | Self::EmptyHideFailed(window) => {
                Some(window)
            }
        }
    }

    /// Shows the overlay while `sample` has something to paint and hides it otherwise,
    /// keeping `frames.shown` true exactly while it's `Shown`.
    pub(super) fn sync<S: Surface<Window = W>>(
        &mut self,
        surface: &mut S,
        frames: &RefCell<Frames>,
        sample: Sample,
    ) -> Result<(), OpenError> {
        if sample.visible {
            self.show(surface, frames, sample)
        } else {
            self.hide(surface, frames)
        }
    }

    fn show<S: Surface<Window = W>>(
        &mut self,
        surface: &mut S,
        frames: &RefCell<Frames>,
        sample: Sample,
    ) -> Result<(), OpenError> {
        let window = match mem::replace(self, Self::Closed) {
            Self::Shown(window) => {
                if sample.changed {
                    surface.repaint(&window);
                }
                *self = Self::Shown(window);
                return Ok(());
            }
            #[cfg(test)]
            Self::Hidden(window) => window,
            Self::Closed => surface.open()?,
            // Its last show or hide may not have taken effect, so it may still hold old pixels.
            Self::Showing(stale) | Self::EmptyHideFailed(stale) => {
                surface.close(stale)?;
                surface.open()?
            }
        };
        if let Err(error) = surface.show(&window) {
            *self = Self::Showing(window);
            return Err(error);
        }
        frames.borrow_mut().shown = true;
        surface.repaint(&window);
        *self = Self::Shown(window);
        Ok(())
    }

    fn hide<S: Surface<Window = W>>(
        &mut self,
        surface: &mut S,
        frames: &RefCell<Frames>,
    ) -> Result<(), OpenError> {
        let window = match mem::replace(self, Self::Closed) {
            Self::Shown(window) | Self::Showing(window) => window,
            rest => {
                *self = rest;
                return Ok(());
            }
        };
        frames.borrow_mut().hide();
        // Repaint the empty frames first, so a refused hide leaves nothing to paint.
        surface.repaint(&window);
        if let Err(error) = surface.hide(&window) {
            *self = Self::EmptyHideFailed(window);
            return Err(error);
        }
        // The next show opens a window that never presented this drawing; none waits hidden
        // until then (presence.md, "A screen hold's pointer line").
        surface.close(window)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    use kumochi_scene::{layout::SurfaceGeometry, theme::Theme};

    use super::super::{OpenError, Presence};
    use super::{Surface, Visibility};
    use crate::{
        escape::Panel,
        presenter::{Frames, Presenter, Sample},
        runs::Runs,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Open(u8),
        Show(u8),
        /// The window, and whether the frames counted as shown when it repainted.
        Repaint(u8, bool),
        Hide(u8),
        Close(u8),
    }
    use Call::{Close, Hide, Open, Repaint, Show};

    const SHOW: Sample = Sample {
        visible: true,
        changed: true,
    };
    const SAME: Sample = Sample {
        visible: true,
        changed: false,
    };
    const HIDE: Sample = Sample {
        visible: false,
        changed: true,
    };

    /// Numbers the windows it opens from 1, logs every call, and refuses one exact call.
    struct Fake {
        frames: Rc<RefCell<Frames>>,
        opened: u8,
        calls: Vec<Call>,
        refuse: Option<Call>,
    }

    impl Fake {
        fn new(presenter: &Presenter) -> Self {
            Self {
                frames: presenter.frames(),
                opened: 0,
                calls: Vec::new(),
                refuse: None,
            }
        }

        fn call(&mut self, call: Call) -> Result<(), OpenError> {
            self.calls.push(call);
            if self.refuse == Some(call) {
                self.refuse = None;
                return Err(OpenError::Gpui("refused".into()));
            }
            Ok(())
        }

        /// Syncs through the production lifecycle: whether it succeeded, and its calls.
        fn sync(&mut self, state: &mut Visibility<u8>, sample: Sample) -> (bool, Vec<Call>) {
            let frames = Rc::clone(&self.frames);
            let ok = state.sync(self, &frames, sample).is_ok();
            (ok, std::mem::take(&mut self.calls))
        }

        fn assert_order(&mut self, expected: &[Call]) {
            let calls = std::mem::take(&mut self.calls);
            let mut after = 0;
            for event in expected {
                let Some(index) = calls.iter().position(|call| call == event) else {
                    panic!("{event:?} is missing from {calls:?}");
                };
                assert!(index >= after, "{event:?} out of order in {calls:?}");
                after = index + 1;
            }
        }
    }

    impl Surface for Fake {
        type Window = u8;

        fn open(&mut self) -> Result<u8, OpenError> {
            self.opened += 1;
            self.call(Open(self.opened)).map(|()| self.opened)
        }

        fn show(&mut self, window: &u8) -> Result<(), OpenError> {
            self.call(Show(*window))
        }

        fn repaint(&mut self, window: &u8) {
            let shown = self.frames.borrow().shown;
            self.calls.push(Repaint(*window, shown));
        }

        fn hide(&mut self, window: &u8) -> Result<(), OpenError> {
            self.call(Hide(*window))
        }

        fn close(&mut self, window: u8) -> Result<(), OpenError> {
            self.call(Close(window))
        }
    }

    fn presenter() -> Presenter {
        kumochi_scene::deferred::start().expect("the dropper starts");
        let surface = SurfaceGeometry::Overlay {
            default_canvas: [1920.0, 1080.0],
        };
        Presenter::new(
            surface,
            Arc::new(Theme::default()),
            Box::new(crate::presence::paint_cache::Stager(size_of::<
                crate::presence::paint_cache::Stager,
            >())),
        )
    }

    fn holds_esc(presenter: &Presenter) -> bool {
        Presence::activity(&Runs::default(), Panel::Closed, presenter).holds_global_escape()
    }

    /// Window 1 shown with a drawing.
    fn shown(presenter: &Presenter) -> (Fake, Visibility<u8>) {
        let mut fake = Fake::new(presenter);
        let mut state = Visibility::Closed;
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Open(1), Show(1), Repaint(1, true)])
        );
        (fake, state)
    }

    #[test]
    fn a_hidden_drawing_shows_the_next_in_a_fresh_window() {
        use crate::{presence::paint_cache::Stager, presenter::tests::painter_fixture};
        let (presenter, scene) = painter_fixture(Box::new(Stager(size_of::<Stager>())));
        let frames = presenter.frames();
        let mut fake = Fake::new(&presenter);
        let mut state = Visibility::Closed;
        state.sync(&mut fake, &frames, HIDE).expect("idle hide");
        assert!(
            fake.calls.is_empty(),
            "idle hide does not touch native windows"
        );
        assert_eq!(state.window(), None);
        let assert_activity = |shown| {
            let activity = Presence::activity(&Runs::default(), Panel::Closed, &presenter);
            assert_eq!(activity.drawing, shown);
            assert_eq!(activity.holds_global_escape(), shown);
        };
        assert_activity(false);

        state.sync(&mut fake, &frames, SHOW).expect("first show");
        let first = *state.window().expect("the first host");
        fake.assert_order(&[Open(first), Show(first), Repaint(first, true)]);
        assert_activity(true);
        state
            .sync(&mut fake, &frames, SAME)
            .expect("unchanged sample");
        assert!(fake.calls.is_empty(), "an unchanged sample is quiet");
        state
            .sync(&mut fake, &frames, SHOW)
            .expect("changed sample");
        assert_eq!(state.window(), Some(&first));
        assert!(
            !fake.calls.is_empty(),
            "a changed sample schedules a repaint"
        );
        assert!(fake.calls.iter().all(|call| *call == Repaint(first, true)));
        fake.calls.clear();
        assert!(!frames.borrow().paint(|_, _| Ok::<_, OpenError>(())));
        assert_eq!(
            frames
                .borrow()
                .observations()
                .into_iter()
                .flatten()
                .map(|(id, _, bound)| (id, bound))
                .collect::<Vec<_>>(),
            [(scene, 1)]
        );

        state.sync(&mut fake, &frames, HIDE).expect("hide");
        fake.assert_order(&[Repaint(first, false), Hide(first), Close(first)]);
        assert_eq!(state.window(), None);
        assert_activity(false);
        assert!(frames.borrow().observations().iter().all(Option::is_none));
        state
            .sync(&mut fake, &frames, HIDE)
            .expect("already hidden");
        assert!(fake.calls.is_empty(), "there is no native host to touch");
        state.sync(&mut fake, &frames, SHOW).expect("next show");
        let next = *state.window().expect("a newly shown host");
        assert_ne!(next, first);
        fake.assert_order(&[Open(next), Show(next), Repaint(next, true)]);
        assert_activity(true);
        assert!(
            frames.borrow().observations().iter().all(Option::is_none),
            "no stale paint before the new host draws"
        );
    }

    #[test]
    fn a_refused_hide_stays_empty_without_esc_or_retry_and_never_shows_again() {
        let presenter = presenter();
        let (mut fake, mut state) = shown(&presenter);
        fake.refuse = Some(Hide(1));
        assert_eq!(
            fake.sync(&mut state, HIDE),
            (false, vec![Repaint(1, false), Hide(1)])
        );
        assert_eq!(state, Visibility::EmptyHideFailed(1));
        assert!(!holds_esc(&presenter));
        assert_eq!(fake.sync(&mut state, HIDE), (true, vec![]), "no retry");
        assert_eq!(state, Visibility::EmptyHideFailed(1));
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Close(1), Open(2), Show(2), Repaint(2, true)])
        );
        assert_eq!(state, Visibility::Shown(2));
        assert!(holds_esc(&presenter));
    }

    #[test]
    fn a_failed_hide_that_cannot_close_shows_nothing_until_a_fresh_window() {
        let presenter = presenter();
        let (mut fake, mut state) = shown(&presenter);
        fake.refuse = Some(Hide(1));
        assert_eq!(
            fake.sync(&mut state, HIDE),
            (false, vec![Repaint(1, false), Hide(1)])
        );
        fake.refuse = Some(Close(1));
        assert_eq!(fake.sync(&mut state, SHOW), (false, vec![Close(1)]));
        assert_eq!(state, Visibility::Closed);
        assert!(!holds_esc(&presenter));
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Open(2), Show(2), Repaint(2, true)])
        );
    }

    #[test]
    fn a_refused_show_is_never_trusted_hidden_or_shown() {
        let presenter = presenter();
        let frames = presenter.frames();
        let mut fake = Fake::new(&presenter);
        let mut state = Visibility::Closed;
        fake.refuse = Some(Show(1));
        assert!(matches!(
            state.sync(&mut fake, &frames, SHOW),
            Err(OpenError::Gpui(_))
        ));
        assert!(!presenter.drawn());
        assert!(!holds_esc(&presenter));
        state
            .sync(&mut fake, &frames, HIDE)
            .expect("retire possibly visible host");
        fake.assert_order(&[Open(1), Show(1), Repaint(1, false), Hide(1), Close(1)]);
        assert_eq!(state.window(), None);
        let refused = fake.opened + 1;
        fake.refuse = Some(Show(refused));
        assert!(matches!(
            state.sync(&mut fake, &frames, SHOW),
            Err(OpenError::Gpui(_))
        ));
        assert!(!presenter.drawn());
        assert!(!holds_esc(&presenter));
        state
            .sync(&mut fake, &frames, SHOW)
            .expect("show a fresh host");
        let next = *state.window().expect("the shown host");
        assert_ne!(next, refused);
        fake.assert_order(&[
            Open(refused),
            Show(refused),
            Close(refused),
            Open(next),
            Show(next),
            Repaint(next, true),
        ]);
        assert!(presenter.drawn());
    }

    #[test]
    fn a_hidden_window_that_cannot_be_replaced_leaves_none() {
        let presenter = presenter();
        let frames = presenter.frames();
        let mut fake = Fake::new(&presenter);
        let mut state = Visibility::Closed;
        state.sync(&mut fake, &frames, SHOW).expect("first show");
        let initial = *state.window().expect("first host");
        fake.assert_order(&[Open(initial), Show(initial), Repaint(initial, true)]);
        state.sync(&mut fake, &frames, HIDE).expect("hide");
        fake.assert_order(&[Repaint(initial, false), Hide(initial), Close(initial)]);
        fake.refuse = Some(Open(fake.opened + 1));
        assert!(matches!(
            state.sync(&mut fake, &frames, SHOW),
            Err(OpenError::Gpui(_))
        ));
        assert!(
            !fake
                .calls
                .iter()
                .any(|call| matches!(call, Show(_) | Repaint(_, _)))
        );
        assert_eq!(state.window(), None);
        assert!(!holds_esc(&presenter));
        fake.calls.clear();
        state
            .sync(&mut fake, &frames, SHOW)
            .expect("show after open refusal");
        let earlier = *state.window().expect("the shown host");
        assert_ne!(earlier, initial);
        fake.assert_order(&[Open(earlier), Show(earlier), Repaint(earlier, true)]);
        assert!(presenter.drawn());
        fake.refuse = Some(Close(earlier));
        assert!(matches!(
            state.sync(&mut fake, &frames, HIDE),
            Err(OpenError::Gpui(_))
        ));
        fake.assert_order(&[Repaint(earlier, false), Hide(earlier), Close(earlier)]);
        assert_eq!(state.window(), None);
        assert!(!holds_esc(&presenter));
        state
            .sync(&mut fake, &frames, SHOW)
            .expect("show after close refusal");
        let next = *state.window().expect("a fresh host");
        assert_ne!(next, earlier);
        fake.assert_order(&[Open(next), Show(next), Repaint(next, true)]);
        assert!(presenter.drawn());
    }
}
