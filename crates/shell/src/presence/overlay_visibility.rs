//! The overlay window's visibility, kept apart from the frames' paint eligibility. A window
//! whose show or hide may not have taken effect is never shown again, and a hidden one is
//! replaced, so the next drawing can't appear with the last one's frame still on its surface.

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
    /// No window: none was needed yet, or the last one closed with no replacement.
    Closed,
    /// Opened hidden and never shown.
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
            Self::Hidden(window)
            | Self::Showing(window)
            | Self::Shown(window)
            | Self::EmptyHideFailed(window) => Some(window),
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
        // The next show gets a window that never presented this drawing.
        surface.close(window)?;
        *self = Self::Hidden(surface.open()?);
        Ok(())
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
            logical_size: [1920.0, 1080.0],
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
        let presenter = presenter();
        let mut fake = Fake::new(&presenter);
        let mut state = Visibility::Closed;
        assert_eq!(fake.sync(&mut state, HIDE), (true, vec![]), "nothing opens");
        assert_eq!(state, Visibility::Closed);
        let (mut fake, mut state) = shown(&presenter);
        assert!(holds_esc(&presenter));
        assert_eq!(fake.sync(&mut state, SAME), (true, vec![]));
        assert_eq!(fake.sync(&mut state, SHOW), (true, vec![Repaint(1, true)]));
        assert_eq!(
            fake.sync(&mut state, HIDE),
            (true, vec![Repaint(1, false), Hide(1), Close(1), Open(2)])
        );
        assert_eq!(state, Visibility::Hidden(2));
        assert!(!holds_esc(&presenter));
        assert_eq!(fake.sync(&mut state, HIDE), (true, vec![]));
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Show(2), Repaint(2, true)])
        );
        assert_eq!(state, Visibility::Shown(2));
        assert!(holds_esc(&presenter));
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
        let mut fake = Fake::new(&presenter);
        let mut state = Visibility::Closed;
        fake.refuse = Some(Show(1));
        assert_eq!(fake.sync(&mut state, SHOW), (false, vec![Open(1), Show(1)]));
        assert_eq!(state, Visibility::Showing(1));
        assert!(!holds_esc(&presenter));
        assert_eq!(
            fake.sync(&mut state, HIDE),
            (true, vec![Repaint(1, false), Hide(1), Close(1), Open(2)])
        );
        fake.refuse = Some(Show(2));
        assert_eq!(fake.sync(&mut state, SHOW), (false, vec![Show(2)]));
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Close(2), Open(3), Show(3), Repaint(3, true)])
        );
        assert_eq!(state, Visibility::Shown(3));
    }

    #[test]
    fn a_hidden_window_that_cannot_be_replaced_leaves_none() {
        let presenter = presenter();
        let (mut fake, mut state) = shown(&presenter);
        fake.refuse = Some(Open(2));
        assert_eq!(
            fake.sync(&mut state, HIDE),
            (false, vec![Repaint(1, false), Hide(1), Close(1), Open(2)])
        );
        assert_eq!(state, Visibility::Closed);
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Open(3), Show(3), Repaint(3, true)])
        );
        fake.refuse = Some(Close(3));
        assert_eq!(
            fake.sync(&mut state, HIDE),
            (false, vec![Repaint(3, false), Hide(3), Close(3)])
        );
        assert_eq!(state, Visibility::Closed);
        assert!(!holds_esc(&presenter));
        assert_eq!(
            fake.sync(&mut state, SHOW),
            (true, vec![Open(4), Show(4), Repaint(4, true)])
        );
    }
}
