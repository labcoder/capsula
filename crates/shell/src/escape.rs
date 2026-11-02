//! What Esc does, decided locally before anything reaches the brain.
//!
//! The order is fixed: stop moving work first, clear an idle drawing second, otherwise
//! dismiss the topmost local panel. Esc is held system-wide only while Capsula speaks or
//! shows a drawing, so an app in front keeps it otherwise.

/// What the shell knows is moving or on screen. Speech arrives in slice 7, so `speaking` stays
/// false until then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Activity {
    /// A run streams, speaks or acts.
    pub(crate) working: bool,
    /// Audio is playing.
    pub(crate) speaking: bool,
    /// A drawing is on screen.
    pub(crate) drawing: bool,
}

impl Activity {
    /// Esc is held system-wide only for speech or a drawing, never for Listening or hover.
    pub(crate) fn holds_global_escape(self) -> bool {
        self.speaking || self.drawing
    }
}

/// The one local effect of an Esc press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Escape {
    Stop,
    Clear,
    Dismiss,
    Ignore,
}

/// Whether a local panel Esc could dismiss is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Open,
    Threads,
    Closed,
}

/// Decides one Esc press from what's moving and whether a local panel is open.
pub(crate) fn decide(activity: Activity, panel: Panel) -> Escape {
    if activity.working || activity.speaking {
        Escape::Stop
    } else if activity.drawing {
        Escape::Clear
    } else if panel != Panel::Closed {
        Escape::Dismiss
    } else {
        Escape::Ignore
    }
}

/// Holds global Esc while it's wanted, and shows Stop instead when Windows refuses it.
#[derive(Debug)]
pub(crate) struct EscapeHold<H> {
    hotkey: Option<H>,
    stop: bool,
}

impl<H> Default for EscapeHold<H> {
    fn default() -> Self {
        Self {
            hotkey: None,
            stop: false,
        }
    }
}

impl<H> EscapeHold<H> {
    /// Holds Esc, registering it unless it's already held. A refusal is logged without key or
    /// text content and turns on the Stop fallback.
    pub(crate) fn acquire<E: std::fmt::Display>(
        &mut self,
        register: impl FnOnce() -> Result<H, E>,
    ) {
        if self.hotkey.is_some() {
            return;
        }
        match register() {
            Ok(hotkey) => {
                self.hotkey = Some(hotkey);
                self.stop = false;
            }
            Err(error) => {
                tracing::warn!(%error, "can't hold Esc; showing Stop instead");
                self.stop = true;
            }
        }
    }

    /// Gives Esc back and turns the Stop fallback off.
    pub(crate) fn release(&mut self) {
        self.hotkey = None;
        self.stop = false;
    }

    /// Whether the panel shows Stop in place of global Esc.
    pub(crate) fn shows_stop(&self) -> bool {
        self.stop
    }
}

#[cfg(test)]
mod tests {
    use super::{Activity, Escape, EscapeHold, Panel, decide};

    #[test]
    fn a_focused_threads_panel_dismisses_only_after_stop_and_clear() {
        assert_eq!(
            decide(
                Activity {
                    working: true,
                    ..Activity::default()
                },
                Panel::Threads
            ),
            Escape::Stop
        );
        assert_eq!(
            decide(
                Activity {
                    drawing: true,
                    ..Activity::default()
                },
                Panel::Threads
            ),
            Escape::Clear
        );
        assert_eq!(decide(Activity::default(), Panel::Threads), Escape::Dismiss);
        assert_eq!(decide(Activity::default(), Panel::Closed), Escape::Ignore);
    }

    #[test]
    fn work_stops_before_a_drawing_clears_before_a_panel_dismisses() {
        let all = Activity {
            working: true,
            speaking: true,
            drawing: true,
        };
        assert_eq!(decide(all, Panel::Open), Escape::Stop);
        let speaking = Activity {
            speaking: true,
            ..Activity::default()
        };
        assert_eq!(decide(speaking, Panel::Open), Escape::Stop);
        let drawing = Activity {
            drawing: true,
            ..Activity::default()
        };
        assert_eq!(decide(drawing, Panel::Open), Escape::Clear);
        assert_eq!(decide(Activity::default(), Panel::Open), Escape::Dismiss);
        assert_eq!(decide(Activity::default(), Panel::Closed), Escape::Ignore);
    }

    #[test]
    fn only_speech_or_a_drawing_holds_global_escape() {
        assert!(!Activity::default().holds_global_escape());
        let working = Activity {
            working: true,
            ..Activity::default()
        };
        assert!(!working.holds_global_escape());
        let speaking = Activity {
            speaking: true,
            ..Activity::default()
        };
        assert!(speaking.holds_global_escape());
    }

    #[test]
    fn a_refused_hold_shows_stop_until_esc_is_no_longer_wanted() {
        let mut hold = EscapeHold::<()>::default();
        hold.acquire(|| Err("taken"));
        assert!(hold.shows_stop());
        hold.acquire(|| Ok::<(), &str>(()));
        assert!(!hold.shows_stop());
        hold.release();
        assert!(!hold.shows_stop());
        hold.acquire(|| Err("taken"));
        hold.release();
        assert!(!hold.shows_stop());
    }
}
