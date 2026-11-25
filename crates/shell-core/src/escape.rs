//! What Esc does, decided locally before anything reaches the brain.
//!
//! The order is fixed: cancel a live pull first, stop moving work second, clear an idle drawing
//! third, otherwise dismiss the topmost local panel. Esc is held system-wide only while Kumochi
//! speaks, shows a drawing or has Kumo pulled out, so an app in front keeps it otherwise.

use std::time::Instant;

use kumochi_protocol::message::RunId;

use crate::runs::Runs;

/// Which surface received the press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechEscapeOwner {
    /// A Kumochi surface keeps its local Esc order.
    Panel,
    /// The native hotkey from outside a Kumochi surface can only stop speech.
    Outside,
}

/// The run and Esc owner observed at the press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechPress {
    /// The link generation that presented the speech.
    pub session: u64,
    /// The run that presented the speech.
    pub run: RunId,
    /// The owner at input delivery, even if focus changes before handling.
    pub owner: SpeechEscapeOwner,
}

impl SpeechPress {
    /// Captures the target and owner before a delivery can wait on the UI.
    pub fn capture(
        lease: Option<(u64, RunId, Instant)>,
        now: Instant,
        owner: SpeechEscapeOwner,
    ) -> Option<Self> {
        let (session, run, until) = lease?;
        (until > now).then_some(Self {
            session,
            run,
            owner,
        })
    }

    /// An ended or replaced run can only be stopped at its own speaker.
    pub fn speaker_only(self, runs: &Runs, moving: Option<RunId>) -> bool {
        moving != Some(self.run)
            || runs.state_ended(self.run)
            || (runs.newest_run() >= self.run && runs.stoppable() != Some(self.run))
    }
}

/// What the shell knows is moving or on screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a separate fact Esc's order reads"
)]
pub struct Activity {
    /// A run streams, speaks or acts.
    pub working: bool,
    /// The native route presents a run's audio, and no focused Kumochi panel takes Esc itself.
    pub speaking: bool,
    /// A drawing is on screen.
    pub drawing: bool,
    /// Kumo is being pulled out of the peek.
    pub pulling: bool,
    /// A hold's question is being taken, from its recognition until its run answers.
    pub voice: bool,
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

    /// Whether the current hold owns its Escape registration.
    #[cfg(any(test, feature = "test-support"))]
    pub fn held(&self) -> bool {
        self.hotkey.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use kumochi_protocol::message::{DataToShell, Phase, RunId, ThreadId};

    use super::{
        Activity, Escape, EscapeHold, Panel, SpeechEscapeOwner, SpeechPress, TalkEscape,
        TalkEscapeRefused, decide,
    };
    use crate::runs::Runs;

    #[test]
    fn focused_panel_owns_native_escape_even_when_a_pull_keeps_the_hotkey() {
        let now = Instant::now();
        let lease = Some((1, RunId(5), now + Duration::from_millis(400)));
        let activity = Activity {
            pulling: true,
            ..Activity::default()
        };
        assert!(activity.holds_global_escape());
        let press =
            SpeechPress::capture(lease, now, SpeechEscapeOwner::Panel).expect("live speech");
        assert_eq!(
            SpeechPress::capture(lease, now, SpeechEscapeOwner::Outside)
                .expect("live speech")
                .owner,
            SpeechEscapeOwner::Outside
        );
        assert_eq!(
            press.owner,
            SpeechEscapeOwner::Panel,
            "the pull did not give Esc to speech"
        );
        assert_eq!(decide(activity, Panel::Open), Escape::CancelPull);
        assert_eq!(
            SpeechPress::capture(
                lease,
                now + Duration::from_millis(400),
                SpeechEscapeOwner::Panel
            ),
            None
        );
    }

    #[test]
    fn focused_escape_stops_terminal_audio_without_stopping_a_brain_run() {
        let mut runs = Runs::default();
        let press = SpeechPress {
            session: 1,
            run: RunId(5),
            owner: SpeechEscapeOwner::Panel,
        };
        runs.apply_state(press.run, ThreadId(1), Phase::Answering, None);
        assert!(!press.speaker_only(&runs, runs.stoppable()));
        runs.apply_state(press.run, ThreadId(1), Phase::Done, None);
        assert_eq!(runs.stoppable(), Some(press.run), "RunEnd has not arrived");
        assert!(runs.state_ended(press.run));
        assert!(press.speaker_only(&runs, runs.stoppable()));
        runs.apply_data(DataToShell::RunEnd { run: press.run }, 0);
        assert_eq!(runs.take_ended(), Some(press.run));
        assert!(
            press.speaker_only(&runs, Some(press.run)),
            "an ended run's drawing does not make its audio a live run again"
        );
        runs.apply_state(RunId(6), ThreadId(1), Phase::Answering, None);
        assert_eq!(runs.stoppable(), Some(RunId(6)));
        assert!(press.speaker_only(&runs, runs.stoppable()));
    }

    #[test]
    fn work_stops_before_a_drawing_clears_before_a_panel_dismisses() {
        let all = Activity {
            working: true,
            speaking: true,
            drawing: true,
            pulling: false,
            voice: false,
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
        let working = Activity {
            working: true,
            ..Activity::default()
        };
        for (panel, idle) in [
            (Panel::Open, Escape::Dismiss),
            (Panel::Threads, Escape::Dismiss),
            (Panel::Settings, Escape::Dismiss),
            (Panel::Rename, Escape::CancelEdit),
            (Panel::SettingsEdit, Escape::CancelEdit),
            (Panel::Onboarding, Escape::Dismiss),
            (Panel::OnboardingEdit, Escape::CancelEdit),
        ] {
            assert_eq!(decide(Activity::default(), panel), idle, "{panel:?}");
            assert_eq!(decide(drawing, panel), Escape::Clear, "{panel:?}");
            assert_eq!(decide(working, panel), Escape::Stop, "{panel:?}");
        }
        for panel in [Panel::Onboarding, Panel::OnboardingEdit] {
            let working_and_drawing = Activity {
                working: true,
                ..drawing
            };
            assert_eq!(decide(working_and_drawing, panel), Escape::Stop);
            assert_eq!(
                decide(
                    Activity {
                        pulling: true,
                        ..working_and_drawing
                    },
                    panel
                ),
                Escape::CancelPull
            );
        }
        assert_eq!(decide(Activity::default(), Panel::Closed), Escape::Ignore);
    }

    #[test]
    fn a_live_pull_cancels_before_anything_else_and_holds_global_escape() {
        let all = Activity {
            working: true,
            speaking: true,
            drawing: true,
            pulling: true,
            voice: false,
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
    fn only_speech_a_drawing_or_a_pull_holds_global_escape() {
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
        assert!(
            Activity {
                drawing: true,
                ..Activity::default()
            }
            .holds_global_escape()
        );
        assert!(
            Activity {
                pulling: true,
                ..Activity::default()
            }
            .holds_global_escape()
        );
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
        chord.release();
        assert_eq!((asked.get(), freed.get()), (1, 1), "its keys are up");
        chord.acquire(register).unwrap();
        assert_eq!((asked.get(), freed.get()), (2, 1), "the next hold");
    }

    #[test]
    fn a_refused_esc_with_the_talk_keys_is_not_asked_for_again_until_the_next_hold() {
        let (asked, freed) = (Cell::new(0), Rc::new(Cell::new(0)));
        let register = || {
            asked.set(asked.get() + 1);
            Ok::<_, &str>(Held(Rc::clone(&freed)))
        };
        let mut chord = TalkEscape::default();
        assert_eq!(
            chord.acquire(|| {
                asked.set(asked.get() + 1);
                Err::<Held, _>("taken")
            }),
            Err(TalkEscapeRefused)
        );
        assert_eq!(chord.acquire(register), Err(TalkEscapeRefused));
        assert_eq!((asked.get(), freed.get()), (1, 0));
        chord.release();
        assert_eq!(chord.acquire(register), Ok(()));
        assert_eq!((asked.get(), freed.get()), (2, 0));
        chord.release();
        assert_eq!((asked.get(), freed.get()), (2, 1));
    }
}
