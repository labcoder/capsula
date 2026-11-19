//! What Esc does, decided locally before anything reaches the brain.
//!
//! The order is fixed: cancel a live pull first, stop moving work second, clear an idle drawing
//! third, otherwise dismiss the topmost local panel. Esc is held system-wide only while Kumochi
//! speaks, shows a drawing or has Kumo pulled out, so an app in front keeps it otherwise.

/// What the shell knows is moving or on screen. Speech arrives in slice 7, so `speaking` stays
/// false until then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a separate fact Esc's order reads"
)]
pub struct Activity {
    /// A run streams, speaks or acts.
    pub working: bool,
    /// Audio is playing.
    pub speaking: bool,
    /// A drawing is on screen.
    pub drawing: bool,
    /// Kumo is being pulled out of the peek.
    pub pulling: bool,
}

impl Activity {
    /// Esc is held system-wide only for speech, a drawing or a pull, never for Listening or
    /// hover.
    pub fn holds_global_escape(self) -> bool {
        self.speaking || self.drawing || self.pulling
    }
}

/// The one local effect of an Esc press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    CancelPull,
    Stop,
    Clear,
    Dismiss,
    CancelEdit,
    Ignore,
}

/// Whether a local panel Esc could dismiss is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Open,
    Threads,
    Rename,
    Settings,
    SettingsEdit,
    /// The onboarding window, with no unsaved name edit.
    Onboarding,
    /// The onboarding window with an unsaved name edit.
    OnboardingEdit,
    Closed,
}

/// Decides one Esc press from what's moving and whether a local panel is open.
pub fn decide(activity: Activity, panel: Panel) -> Escape {
    if activity.pulling {
        Escape::CancelPull
    } else if activity.working || activity.speaking {
        Escape::Stop
    } else if activity.drawing {
        Escape::Clear
    } else if matches!(
        panel,
        Panel::Rename | Panel::SettingsEdit | Panel::OnboardingEdit
    ) {
        Escape::CancelEdit
    } else if panel != Panel::Closed {
        Escape::Dismiss
    } else {
        Escape::Ignore
    }
}

/// Holds global Esc while it's wanted, and shows Stop instead when Windows refuses it.
#[derive(Debug)]
pub struct EscapeHold<H> {
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
    pub fn acquire<E: std::fmt::Display>(&mut self, register: impl FnOnce() -> Result<H, E>) {
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
    pub fn release(&mut self) {
        self.hotkey = None;
        self.stop = false;
    }

    /// Whether the panel shows Stop in place of global Esc.
    pub fn shows_stop(&self) -> bool {
        self.stop
    }
}

/// Holds Esc with the talk keys down while a hold's keys are down, so that press cancels the
/// hold and Windows doesn't act on it as well.
#[derive(Debug)]
pub struct TalkEscape<H> {
    hotkey: Option<H>,
    refused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the talk Escape lease is unavailable")]
pub struct TalkEscapeRefused;

impl<H> Default for TalkEscape<H> {
    fn default() -> Self {
        Self {
            hotkey: None,
            refused: false,
        }
    }
}

impl<H> TalkEscape<H> {
    /// Tries once for the hold. Refusal leaves it without any cancellation lease.
    pub fn acquire<E>(
        &mut self,
        register: impl FnOnce() -> Result<H, E>,
    ) -> Result<(), TalkEscapeRefused> {
        if self.hotkey.is_none() && !self.refused {
            self.hotkey = register().ok();
            self.refused = self.hotkey.is_none();
        }
        if self.refused {
            Err(TalkEscapeRefused)
        } else {
            Ok(())
        }
    }

    /// Gives the chord back once the hold's keys are up or it ended.
    pub fn release(&mut self) {
        self.hotkey = None;
        self.refused = false;
    }

    /// Whether Windows refused the chord for the hold whose keys are down.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refused(&self) -> bool {
        self.refused
    }

    #[cfg(feature = "test-support")]
    pub fn held(&self) -> bool {
        self.hotkey.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::{Activity, Escape, EscapeHold, Panel, TalkEscape, TalkEscapeRefused, decide};

    #[test]
    fn onboarding_cancels_an_edit_or_dismisses_only_after_pull_stop_and_clear() {
        for (panel, idle) in [
            (Panel::OnboardingEdit, Escape::CancelEdit),
            (Panel::Onboarding, Escape::Dismiss),
        ] {
            assert_eq!(decide(Activity::default(), panel), idle);
            for (activity, first) in [
                (
                    Activity {
                        pulling: true,
                        working: true,
                        drawing: true,
                        ..Activity::default()
                    },
                    Escape::CancelPull,
                ),
                (
                    Activity {
                        working: true,
                        drawing: true,
                        ..Activity::default()
                    },
                    Escape::Stop,
                ),
                (
                    Activity {
                        drawing: true,
                        ..Activity::default()
                    },
                    Escape::Clear,
                ),
            ] {
                assert_eq!(decide(activity, panel), first);
            }
        }
    }

    #[test]
    fn settings_while_idle_dismisses() {
        assert_eq!(
            decide(Activity::default(), Panel::Settings),
            Escape::Dismiss
        );
    }

    #[test]
    fn settings_edit_keeps_stop_and_clear_before_cancel() {
        assert_eq!(
            decide(
                Activity {
                    working: true,
                    ..Activity::default()
                },
                Panel::SettingsEdit
            ),
            Escape::Stop
        );
        assert_eq!(
            decide(
                Activity {
                    drawing: true,
                    ..Activity::default()
                },
                Panel::SettingsEdit
            ),
            Escape::Clear
        );
        assert_eq!(
            decide(Activity::default(), Panel::SettingsEdit),
            Escape::CancelEdit
        );
    }

    #[test]
    fn work_stops_before_an_edit_cancels() {
        assert_eq!(
            decide(
                Activity {
                    working: true,
                    ..Activity::default()
                },
                Panel::Rename,
            ),
            Escape::Stop,
        );
    }

    #[test]
    fn a_drawing_clears_before_an_edit_cancels() {
        assert_eq!(
            decide(
                Activity {
                    drawing: true,
                    ..Activity::default()
                },
                Panel::Rename,
            ),
            Escape::Clear,
        );
    }

    #[test]
    fn an_idle_edit_cancels_before_the_panel_dismisses() {
        assert_eq!(
            decide(Activity::default(), Panel::Rename),
            Escape::CancelEdit,
        );
        assert_eq!(decide(Activity::default(), Panel::Threads), Escape::Dismiss);
    }

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
            pulling: false,
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
    fn a_live_pull_cancels_before_anything_else_and_holds_global_escape() {
        let all = Activity {
            working: true,
            speaking: true,
            drawing: true,
            pulling: true,
        };
        assert_eq!(decide(all, Panel::SettingsEdit), Escape::CancelPull);
        let pulling = Activity {
            pulling: true,
            ..Activity::default()
        };
        assert_eq!(decide(pulling, Panel::Closed), Escape::CancelPull);
        assert!(pulling.holds_global_escape());
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

    struct Held(Rc<Cell<u32>>);

    impl Drop for Held {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn esc_with_the_talk_keys_is_held_once_a_hold_and_given_back_when_its_keys_are_up() {
        let (asked, freed) = (Cell::new(0), Rc::new(Cell::new(0)));
        let register = || {
            asked.set(asked.get() + 1);
            Ok::<_, &str>(Held(Rc::clone(&freed)))
        };
        let mut chord = TalkEscape::default();
        chord.release();
        assert_eq!((asked.get(), freed.get()), (0, 0), "no hold");
        chord.acquire(register).unwrap();
        chord.acquire(register).unwrap();
        assert_eq!((asked.get(), freed.get()), (1, 0), "one hold");
        assert!(!chord.refused());
        chord.release();
        assert_eq!((asked.get(), freed.get()), (1, 1), "its keys are up");
        chord.acquire(register).unwrap();
        assert_eq!((asked.get(), freed.get()), (2, 1), "the next hold");
    }

    #[test]
    fn a_refused_esc_with_the_talk_keys_is_not_asked_for_again_until_the_next_hold() {
        let asked = Cell::new(0);
        let register = || {
            asked.set(asked.get() + 1);
            Err::<(), _>("taken")
        };
        let mut chord = TalkEscape::default();
        assert_eq!(chord.acquire(register), Err(TalkEscapeRefused));
        assert_eq!(chord.acquire(register), Err(TalkEscapeRefused));
        assert_eq!(asked.get(), 1);
        assert!(chord.refused());
        chord.release();
        assert!(!chord.refused());
        assert_eq!(chord.acquire(register), Err(TalkEscapeRefused));
        assert_eq!(asked.get(), 2);
        assert!(chord.refused());
    }
}
