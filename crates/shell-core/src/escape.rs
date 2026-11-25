//! What Esc does, decided locally before anything reaches the brain.
//!
//! The order is fixed: cancel a live pull first, stop moving work second, clear an idle drawing
//! third, otherwise dismiss the topmost local panel. Esc is held system-wide only while Kumochi
//! speaks, shows a drawing or has Kumo pulled out, so an app in front keeps it otherwise.

use std::time::Instant;

use kumochi_protocol::message::{
    ControlToBrain, GestureId, MAX_VISIBLE_SCENES, RunId, SceneId, ThreadId,
};

use crate::presence::drag::Generation;
use crate::presenter::{Broken, Effect, Presenter};
use crate::runs::Runs;
use crate::settings::Field;

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

    /// Whether `pair`, the open pair's generation, is the one that presented the speech.
    pub fn on(self, pair: Option<u64>) -> bool {
        pair == Some(self.session)
    }

    /// Where a delivered press goes first: the outside hotkey only stops speech, and a panel's
    /// press on a pair that's gone does nothing.
    pub fn route(self, pair: Option<u64>) -> SpeechRoute {
        match self.owner {
            SpeechEscapeOwner::Outside => SpeechRoute::Outside,
            SpeechEscapeOwner::Panel if !self.on(pair) => SpeechRoute::Stale,
            SpeechEscapeOwner::Panel => SpeechRoute::Current,
        }
    }

    /// The stop of this press's speech on its own pair. `Done` comes on control before `RunEnd`
    /// on data, so an ended run can still speak; only its speaker stops.
    pub fn stop(self, runs: &Runs, moving: Option<RunId>) -> SpeechStop {
        if self.speaker_only(runs, moving) {
            SpeechStop::Speaker
        } else {
            SpeechStop::Run
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechRoute {
    Outside,
    Stale,
    Current,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechStop {
    /// Only the speaker stops; no brain run is stopped.
    Speaker,
    /// The run is fenced and the brain told, then its speaker stops.
    Run,
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

/// [`decide`], with a drawing the presenter holds counted while the typing panel is open.
pub fn local_decision(mut activity: Activity, panel: Panel, presenter: &Presenter) -> Escape {
    activity.drawing |= matches!(panel, Panel::Open) && presenter.holds_scenes();
    decide(activity, panel)
}

/// The run Esc's stop would stop at `now`: the moving run, else the one whose drawing still
/// moves.
pub fn moving_run(runs: &Runs, presenter: &Presenter, now: Instant) -> Option<RunId> {
    runs.stoppable()
        .map(|run| drawing_stop_target(presenter, run, now).unwrap_or(run))
        .or_else(|| {
            let moving = presenter.live_work(now);
            presenter.stoppable_run().filter(|_| moving)
        })
}

fn drawing_stop_target(presenter: &Presenter, run: RunId, now: Instant) -> Option<RunId> {
    (presenter.holds_scenes() || presenter.live_work(now)).then(|| {
        presenter
            .stoppable_run()
            .map_or(run, |drawing| drawing.max(run))
    })
}

pub enum RunStop {
    Tell(ControlToBrain),
    PlaceAndStop(RunId),
}

pub fn run_stop(presenter: &Presenter, message: ControlToBrain, now: Instant) -> RunStop {
    if let ControlToBrain::Stop { run, .. } = message
        && let Some(run) = drawing_stop_target(presenter, run, now)
    {
        RunStop::PlaceAndStop(run)
    } else {
        RunStop::Tell(message)
    }
}

/// Esc's ordinary stop, once nothing ahead of it took the press.
pub enum MovingStop {
    /// The moving run is fenced; resolve its stop against the drawings at delivery.
    Run(ControlToBrain),
    /// Only a drawing moves: place the drawings, then [`stop_drawing`].
    Drawing,
    Idle,
}

pub fn stop_moving(runs: &mut Runs, presenter: &Presenter, now: Instant) -> MovingStop {
    match runs.stop() {
        Some(message) => MovingStop::Run(message),
        None if presenter.live_work(now) => MovingStop::Drawing,
        None => MovingStop::Idle,
    }
}

/// Stops the moving drawing's run, or with no run to stop discards its replacement (`None`).
pub fn stop_drawing(
    presenter: &mut Presenter,
    now: Instant,
) -> Option<Result<Vec<Effect>, Broken>> {
    let stopped = presenter
        .stoppable_run()
        .map(|run| presenter.stop(run, now));
    if stopped.is_none() {
        presenter.discard_replacement();
    }
    stopped
}

/// Esc's clear: grant `credit` first, then clear what holds the scenes.
pub struct Cleared {
    pub credit: u32,
    pub then: AfterClear,
}

pub enum AfterClear {
    /// The presenter holds them: its own clear tells the brain.
    Presenter,
    /// Tell the brain the runs' `ClearDrawing`, if any.
    Tell(Option<ControlToBrain>),
}

pub fn clear(runs: &mut Runs, presenter: &Presenter) -> Cleared {
    let effect = runs.clear();
    Cleared {
        credit: effect.credit,
        then: if presenter.holds_scenes() {
            AfterClear::Presenter
        } else {
            AfterClear::Tell(effect.clear)
        },
    }
}

/// One exact run's stop at the presenter, for a press or a hold that names that run.
pub struct OwnedStop {
    /// A drawing is on screen or still moving, so its `StopScenes` tells the brain and the
    /// run's own `Stop` stays unsent.
    pub scenes: bool,
    pub effects: Result<Vec<Effect>, Broken>,
}

/// Stops `run`'s owned scenes. With no drawing on screen or moving, the scene stop stays
/// local and the caller tells the brain the run's `Stop` instead.
pub fn stop_owned(presenter: &mut Presenter, run: RunId, now: Instant) -> OwnedStop {
    let scenes = presenter.holds_scenes() || presenter.live_work(now);
    let effects = presenter.stop_owned(run, now).map(|mut effects| {
        if !scenes {
            effects.retain(|effect| {
                !matches!(effect, Effect::Control(ControlToBrain::StopScenes { .. }))
            });
        }
        effects
    });
    OwnedStop { scenes, effects }
}

/// A panel edit a press may cancel, with the edit's epoch, so a replacement edit is a different
/// owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Name(u64, Field),
    Rename(u64, ThreadId),
}

/// A running answer's marks, seen when the press was published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkedAnswer {
    pub run: RunId,
    pub scene: SceneId,
    /// The presenter's other stoppable work, read only when this press would stop the answer.
    pub other_work: Option<RunId>,
}

impl MarkedAnswer {
    /// The marks of `run` in `scene`. `other_work` is read only when `action` stops that same
    /// run with no hold taking the press.
    pub fn new(
        (run, scene): (RunId, SceneId),
        action: Escape,
        stoppable: Option<RunId>,
        hold: Option<GestureId>,
        other_work: impl FnOnce() -> Option<RunId>,
    ) -> Self {
        Self {
            run,
            scene,
            other_work: (matches!(action, Escape::Stop | Escape::CancelPull)
                && stoppable == Some(run)
                && hold.is_none())
            .then(other_work)
            .flatten(),
        }
    }
}

/// The owners the aggregate plain Esc route may affect, excluding paint and phase progress. A
/// panel's window is an opaque comparable `W`, and a hold's screen watch an opaque `M`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlainEscape<W, M> {
    pub pair: Option<u64>,
    pub target: (Option<ThreadId>, u64),
    pub frontier: (u32, RunId),
    pub moving: (Option<RunId>, Option<RunId>),
    pub action: Escape,
    pub scenes: [[Option<SceneId>; MAX_VISIBLE_SCENES]; 2],
    pub hold: Option<GestureId>,
    pub watch: Option<M>,
    pub marks: Option<MarkedAnswer>,
    pub selection: Option<GestureId>,
    pub pull: Option<Generation>,
    pub panel: Option<(Panel, W, u64)>,
    pub edit: Option<Edit>,
}

/// How a published press still matches the live owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlainMatch {
    Unchanged,
    /// Only the marks of this running answer went, as Esc's first effect does; the press still
    /// stops that answer.
    MarkGone(RunId),
}

impl<W: Copy + Eq, M: Copy + Eq> PlainEscape<W, M> {
    /// The scope with its panel's live edit: a dismiss that finds an edit cancels that edit.
    #[must_use]
    pub fn with_edit(self, edit: Option<Edit>) -> Self {
        Self {
            action: if edit.is_some() {
                Escape::CancelEdit
            } else {
                self.action
            },
            edit,
            ..self
        }
    }

    /// False when an onboarding dismiss or edit cancel names a name edit onboarding no longer
    /// has. `live` is read only for that case.
    pub fn onboarding_current(&self, live: impl FnOnce() -> Option<Field>) -> bool {
        let field = match self.edit {
            Some(Edit::Name(_, field)) => Some(field),
            _ => None,
        };
        !(matches!(self.action, Escape::Dismiss | Escape::CancelEdit)
            && matches!(self.panel, Some((Panel::Onboarding, ..)))
            && field != live())
    }

    /// Whether this press, published before it waited, may act on `live`.
    pub fn matches(&self, live: &Self) -> Option<PlainMatch> {
        if self == live {
            return Some(PlainMatch::Unchanged);
        }
        let MarkedAnswer {
            run,
            scene,
            other_work,
        } = self.marks?;
        if !matches!(self.action, Escape::Stop | Escape::CancelPull)
            || self.hold.is_some()
            || self.moving.0 != Some(run)
            || !self.scenes[1].contains(&Some(scene))
        {
            return None;
        }
        let [legacy, presented] = self.scenes;
        let mut remaining = presented.into_iter().filter(|&id| id != Some(scene));
        let scenes = [legacy, std::array::from_fn(|_| remaining.next().flatten())];
        (Self {
            marks: None,
            scenes,
            moving: (Some(run), other_work),
            ..*self
        } == *live)
            .then_some(PlainMatch::MarkGone(run))
    }
}

/// One local press: its speech target and the running answer whose marks it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub speech: Option<SpeechPress>,
    /// The marked run the same press stops: Esc's stop target, and the speech's own live run
    /// when the press targets speech.
    pub answer: Option<RunId>,
}

/// What a press does before its local decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lead {
    /// A hold's question is being taken: the press cancels the hold and nothing else.
    CancelHold,
    /// The selection ends first; the press goes on only to stop the answer whose marks it took.
    EndSelection { goes_on: bool },
    /// Nothing comes first.
    Decide,
}

/// The stop that follows a cancelled pull, once that cancel has applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterPull {
    Speech(SpeechPress),
    /// Esc's stop of the answer, read from the runs after the cancel.
    Run,
}

impl Press {
    /// Reads the answer before the press takes any marks. `marked` is the run whose marks are
    /// up, or whose marks went while the press waited.
    pub fn new(speech: Option<SpeechPress>, marked: Option<RunId>, runs: &Runs) -> Self {
        let answer = marked.filter(|run| {
            runs.stoppable() == Some(*run)
                && speech.is_none_or(|press| press.run == *run && !runs.state_ended(*run))
        });
        Self { speech, answer }
    }

    /// Read after the marks went: a hold being taken comes first, then a selection.
    pub fn lead(self, hold: Option<GestureId>, selection: Option<GestureId>) -> Lead {
        if hold.is_some() {
            Lead::CancelHold
        } else if selection.is_some() {
            Lead::EndSelection {
                goes_on: self.answer.is_some(),
            }
        } else {
            Lead::Decide
        }
    }

    /// Only an answer whose marks this press took is stopped after a pull's cancel.
    pub fn after_pull(self) -> Option<AfterPull> {
        self.answer?;
        Some(self.speech.map_or(AfterPull::Run, AfterPull::Speech))
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

    use kumochi_protocol::message::{
        ControlToBrain, DataToShell, GestureId, MAX_VISIBLE_SCENES, Phase, RunId, SceneId, Step,
        ThreadId,
    };

    use super::{
        Activity, AfterClear, AfterPull, Edit, Escape, EscapeHold, Lead, MarkedAnswer, MovingStop,
        Panel, PlainEscape, PlainMatch, Press, RunStop, SpeechEscapeOwner, SpeechPress,
        SpeechRoute, SpeechStop, TalkEscape, TalkEscapeRefused, clear, decide, moving_run,
        run_stop, stop_drawing, stop_moving,
    };
    use crate::presenter::fixture::{begin, commit_as, live, presenter, rect};
    use crate::runs::Runs;
    use crate::settings::Field;

    const ANSWER: RunId = RunId(5);
    const MARKED: SceneId = SceneId(7);

    #[test]
    fn an_ordinary_stop_fences_the_moving_run_and_then_finds_nothing_to_stop() {
        let mut runs = Runs::default();
        let mut presenter = presenter();
        runs.apply_state(ANSWER, ThreadId(1), Phase::Answering, None);
        let now = Instant::now();
        assert_eq!(moving_run(&runs, &presenter, now), Some(ANSWER));
        let MovingStop::Run(message) = stop_moving(&mut runs, &presenter, now) else {
            panic!("the moving run is stopped first");
        };
        let RunStop::Tell(ControlToBrain::Stop { run, revealed }) =
            run_stop(&presenter, message, now)
        else {
            panic!("without drawings the original Stop goes to the brain");
        };
        assert_eq!((run, revealed), (ANSWER, Vec::new()));
        assert_eq!(moving_run(&runs, &presenter, now), None, "fenced");
        assert!(matches!(
            stop_moving(&mut runs, &presenter, now),
            MovingStop::Idle
        ));
        assert!(
            stop_drawing(&mut presenter, now).is_none(),
            "no drawing run to stop"
        );
    }

    #[test]
    fn a_newer_live_drawing_owns_the_stop_of_an_older_run() {
        let (mut presenter, now) = live();
        let drawing = RunId(6);
        begin(&mut presenter, MARKED, now);
        commit_as(&mut presenter, drawing, MARKED, 0, rect("target"), now)
            .expect("a newer drawing");
        let mut runs = Runs::default();
        runs.apply_state(ANSWER, ThreadId(1), Phase::Answering, None);

        assert_eq!(moving_run(&runs, &presenter, now), Some(drawing));
        let MovingStop::Run(message) = stop_moving(&mut runs, &presenter, now) else {
            panic!("fence the older run before resolving the drawing");
        };
        let RunStop::PlaceAndStop(target) = run_stop(&presenter, message, now) else {
            panic!("the live drawing selects a presenter stop");
        };
        assert_eq!(target, drawing);
        assert_eq!(runs.stoppable(), None);
    }

    #[test]
    fn a_clear_with_no_scenes_at_the_presenter_tells_the_runs_clear() {
        let mut runs = Runs::default();
        let presenter = presenter();
        runs.apply_state(ANSWER, ThreadId(1), Phase::Answering, None);
        runs.apply_data(
            DataToShell::StepCommit {
                run: ANSWER,
                scene: MARKED,
                index: 0,
                step: Step {
                    label: "drawing".into(),
                },
            },
            40,
        );
        let cleared = clear(&mut runs, &presenter);
        assert_eq!(cleared.credit, 0);
        let AfterClear::Tell(Some(ControlToBrain::ClearDrawing { scene_ids })) = cleared.then
        else {
            panic!("clear the scene through the runs");
        };
        assert_eq!(scene_ids, vec![MARKED]);
        assert!(!runs.drawing());
    }

    #[test]
    fn a_speech_press_goes_outside_off_its_pair_or_on_and_stops_only_its_speaker_once_ended() {
        let press = |owner| SpeechPress {
            session: 1,
            run: ANSWER,
            owner,
        };
        let (panel, outside) = (
            press(SpeechEscapeOwner::Panel),
            press(SpeechEscapeOwner::Outside),
        );
        assert!(panel.on(Some(1)));
        assert!(!panel.on(Some(2)));
        assert!(!panel.on(None));
        assert_eq!(outside.route(None), SpeechRoute::Outside);
        assert_eq!(outside.route(Some(2)), SpeechRoute::Outside);
        assert_eq!(panel.route(Some(1)), SpeechRoute::Current);
        assert_eq!(
            panel.route(Some(2)),
            SpeechRoute::Stale,
            "a replacement pair"
        );
        assert_eq!(panel.route(None), SpeechRoute::Stale);
        let mut runs = Runs::default();
        runs.apply_state(ANSWER, ThreadId(1), Phase::Answering, None);
        assert_eq!(panel.stop(&runs, Some(ANSWER)), SpeechStop::Run);
        assert_eq!(
            panel.stop(&runs, Some(RunId(6))),
            SpeechStop::Speaker,
            "another run moves"
        );
        runs.apply_state(ANSWER, ThreadId(1), Phase::Done, None);
        assert_eq!(
            panel.stop(&runs, Some(ANSWER)),
            SpeechStop::Speaker,
            "terminal audio"
        );
    }
    const OTHER: SceneId = SceneId(8);

    fn presented(scenes: &[SceneId]) -> [Option<SceneId>; MAX_VISIBLE_SCENES] {
        std::array::from_fn(|index| scenes.get(index).copied())
    }

    /// A Stop press over a running answer, published while its marks were up in the Open panel.
    fn published() -> PlainEscape<u8, u8> {
        PlainEscape {
            pair: Some(1),
            target: (Some(ThreadId(1)), 3),
            frontier: (2, ANSWER),
            moving: (Some(ANSWER), Some(ANSWER)),
            action: Escape::Stop,
            scenes: [presented(&[]), presented(&[MARKED, OTHER])],
            hold: None,
            watch: None,
            marks: Some(MarkedAnswer {
                run: ANSWER,
                scene: MARKED,
                other_work: Some(RunId(4)),
            }),
            selection: None,
            pull: None,
            panel: Some((Panel::Open, 1, 0)),
            edit: None,
        }
    }

    /// The same owners once the press's first effect took the answer's marks.
    fn marks_gone(press: &PlainEscape<u8, u8>) -> PlainEscape<u8, u8> {
        PlainEscape {
            marks: None,
            scenes: [presented(&[]), presented(&[OTHER])],
            moving: (Some(ANSWER), Some(RunId(4))),
            ..*press
        }
    }

    #[test]
    fn a_published_press_acts_only_on_the_owners_it_saw() {
        let press = published();
        assert_eq!(press.matches(&press), Some(PlainMatch::Unchanged));
        for (owner, live) in [
            (
                "an unnamed successor turn",
                PlainEscape {
                    frontier: (3, ANSWER),
                    ..press
                },
            ),
            (
                "a replacement pair",
                PlainEscape {
                    pair: Some(2),
                    ..press
                },
            ),
            (
                "a reopened panel",
                PlainEscape {
                    panel: Some((Panel::Open, 1, 1)),
                    ..press
                },
            ),
            (
                "another window",
                PlainEscape {
                    panel: Some((Panel::Open, 2, 0)),
                    ..press
                },
            ),
            (
                "a new hold",
                PlainEscape {
                    hold: Some(GestureId(9)),
                    ..press
                },
            ),
            (
                "a new selection",
                PlainEscape {
                    selection: Some(GestureId(9)),
                    ..press
                },
            ),
        ] {
            assert_eq!(press.matches(&live), None, "{owner}");
        }
        let edit = |epoch| {
            PlainEscape {
                action: Escape::Dismiss,
                marks: None,
                panel: Some((Panel::Settings, 1, 0)),
                ..press
            }
            .with_edit(Some(Edit::Name(epoch, Field::User)))
        };
        assert_eq!(edit(1).matches(&edit(1)), Some(PlainMatch::Unchanged));
        assert_eq!(edit(1).matches(&edit(2)), None, "a replacement edit");
    }

    #[test]
    fn a_press_that_took_an_answers_marks_still_stops_that_answer_in_either_wake_order() {
        let press = published();
        assert_eq!(
            press.matches(&press),
            Some(PlainMatch::Unchanged),
            "the marks are still up when the press is handled"
        );
        assert_eq!(
            press.matches(&marks_gone(&press)),
            Some(PlainMatch::MarkGone(ANSWER)),
            "the marks went while the press waited"
        );
        let pull = PlainEscape {
            action: Escape::CancelPull,
            ..press
        };
        assert_eq!(
            pull.matches(&marks_gone(&pull)),
            Some(PlainMatch::MarkGone(ANSWER))
        );
        for (owner, press) in [
            (
                "a clear stops no answer",
                PlainEscape {
                    action: Escape::Clear,
                    ..press
                },
            ),
            (
                "a hold owns the press",
                PlainEscape {
                    hold: Some(GestureId(9)),
                    ..press
                },
            ),
            (
                "another run moves",
                PlainEscape {
                    moving: (Some(RunId(6)), Some(ANSWER)),
                    ..press
                },
            ),
        ] {
            assert_eq!(press.matches(&marks_gone(&press)), None, "{owner}");
        }
        let replaced = PlainEscape {
            moving: (Some(ANSWER), Some(RunId(6))),
            ..marks_gone(&press)
        };
        assert_eq!(press.matches(&replaced), None, "the other work changed too");
    }

    #[test]
    fn a_marked_answer_reads_other_work_only_when_the_press_stops_that_answer() {
        let owner = (ANSWER, MARKED);
        let read = |action, stoppable, hold| {
            MarkedAnswer::new(owner, action, stoppable, hold, || Some(RunId(4))).other_work
        };
        assert_eq!(read(Escape::Stop, Some(ANSWER), None), Some(RunId(4)));
        assert_eq!(read(Escape::CancelPull, Some(ANSWER), None), Some(RunId(4)));
        assert_eq!(read(Escape::Clear, Some(ANSWER), None), None);
        assert_eq!(read(Escape::Stop, Some(RunId(6)), None), None);
        assert_eq!(read(Escape::Stop, Some(ANSWER), Some(GestureId(9))), None);
    }

    #[test]
    fn a_dismiss_cancels_a_live_edit_and_onboarding_names_its_own_edit() {
        let dismiss = PlainEscape {
            action: Escape::Dismiss,
            marks: None,
            panel: Some((Panel::Onboarding, 1, 0)),
            ..published()
        };
        assert_eq!(dismiss.with_edit(None).action, Escape::Dismiss);
        let editing = dismiss.with_edit(Some(Edit::Name(1, Field::User)));
        assert_eq!(editing.action, Escape::CancelEdit);
        assert!(editing.onboarding_current(|| Some(Field::User)));
        assert!(!editing.onboarding_current(|| Some(Field::Assistant)));
        assert!(!editing.onboarding_current(|| None), "the edit was saved");
        assert!(dismiss.onboarding_current(|| None));
        assert!(
            !dismiss.onboarding_current(|| Some(Field::User)),
            "a new edit"
        );
        let settings = PlainEscape {
            panel: Some((Panel::Settings, 1, 0)),
            ..editing
        };
        assert!(settings.onboarding_current(|| unreachable!("only onboarding reads it")));
    }

    #[test]
    fn a_press_ends_a_hold_then_a_selection_before_its_local_decision() {
        let answer = Press {
            speech: None,
            answer: Some(ANSWER),
        };
        let plain = Press {
            speech: None,
            answer: None,
        };
        let (hold, selection) = (Some(GestureId(1)), Some(GestureId(2)));
        assert_eq!(answer.lead(hold, selection), Lead::CancelHold);
        assert_eq!(plain.lead(hold, None), Lead::CancelHold);
        assert_eq!(
            answer.lead(None, selection),
            Lead::EndSelection { goes_on: true }
        );
        assert_eq!(
            plain.lead(None, selection),
            Lead::EndSelection { goes_on: false }
        );
        assert_eq!(answer.lead(None, None), Lead::Decide);
    }

    #[test]
    fn only_escs_own_live_target_is_the_marked_answer_and_it_stops_after_a_pull() {
        let mut runs = Runs::default();
        runs.apply_state(ANSWER, ThreadId(1), Phase::Answering, None);
        let speech = |run| SpeechPress {
            session: 1,
            run,
            owner: SpeechEscapeOwner::Panel,
        };
        assert_eq!(Press::new(None, Some(ANSWER), &runs).answer, Some(ANSWER));
        assert_eq!(Press::new(None, Some(RunId(4)), &runs).answer, None);
        assert_eq!(Press::new(None, None, &runs).answer, None);
        assert_eq!(
            Press::new(Some(speech(ANSWER)), Some(ANSWER), &runs).answer,
            Some(ANSWER)
        );
        assert_eq!(
            Press::new(Some(speech(RunId(6))), Some(ANSWER), &runs).answer,
            None,
            "speech from another run"
        );
        assert_eq!(
            Press::new(None, Some(ANSWER), &runs).after_pull(),
            Some(AfterPull::Run)
        );
        let speaking = Press::new(Some(speech(ANSWER)), Some(ANSWER), &runs);
        assert_eq!(
            speaking.after_pull(),
            Some(AfterPull::Speech(speech(ANSWER)))
        );
        assert_eq!(
            Press::new(Some(speech(ANSWER)), None, &runs).after_pull(),
            None
        );
        runs.apply_state(ANSWER, ThreadId(1), Phase::Done, None);
        assert_eq!(runs.stoppable(), Some(ANSWER), "RunEnd has not arrived");
        let tail = Press::new(Some(speech(ANSWER)), Some(ANSWER), &runs);
        assert_eq!(
            tail.answer, None,
            "terminal audio stops at its speaker only"
        );
        assert_eq!(tail.after_pull(), None);
        assert_eq!(
            Press::new(None, Some(ANSWER), &runs).answer,
            Some(ANSWER),
            "the run itself is still Esc's target"
        );
    }

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
