//! What the shell does with link events and the stops it sends, decided apart from the UI.
//! The shell applies the views and schedules the waits; the order lives here.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    AudioSession, ControlToBrain, DataToShell, Phase, RunId, RunOrigin, ThreadId,
};

use super::output::Denied;
use super::{LinkEvent, LinkState};
use crate::presenter::{Broken, Effect, Presenter};
use crate::runs::{Applied, RunDisposition, Runs};

/// How many times a speaker `Stop` is offered to a busy owner before its session ends: a
/// policy, not a measured bound.
pub const STOP_TRIES: u8 = 10;
/// The wait between those offers.
pub const STOP_RETRY: Duration = Duration::from_millis(1);

impl LinkEvent {
    /// Whether this event names a pair other than `current` and is dropped unapplied. Events
    /// that don't name their pair here check it where they apply.
    pub fn stale(&self, current: Option<u64>) -> bool {
        match self {
            Self::RunState { generation, .. }
            | Self::TurnRefused { generation, .. }
            | Self::VoiceHold { generation, .. }
            | Self::CaptureDone { generation, .. }
            | Self::ScreenHolds { generation, .. }
            | Self::HoldScreen { generation, .. }
            | Self::DrawingNotice { generation, .. }
            | Self::CaptureFailed { generation, .. } => current != Some(*generation),
            _ => false,
        }
    }
}

/// What a link state does first, before the shell's models follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lead {
    /// A new connection clears the last refusal.
    pub clears_refusal: bool,
    /// Only a failed link offers Retry.
    pub retry: bool,
}

/// How the shell's models follow a link state, read after its [`Lead`] applied.
#[derive(PartialEq, Eq)]
pub enum Models<'a, S> {
    /// Connected to this pair: runs, presenter and histories meet it.
    Up(&'a S),
    /// Down: every model disconnects, and GitHub shows which way.
    Down(GitHub),
}

impl<S> fmt::Debug for Models<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Up(_) => f.write_str("Up"),
            Self::Down(github) => f.debug_tuple("Down").field(github).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitHub {
    Connecting,
    Disconnected,
}

impl LinkState {
    pub fn lead(self) -> Lead {
        Lead {
            clears_refusal: self == Self::Connected,
            retry: self == Self::Failed,
        }
    }

    /// Only a connected link's pair brings the models up.
    pub fn models<S>(self, session: Option<&S>) -> Models<'_, S> {
        match session.filter(|_| self == Self::Connected) {
            Some(session) => Models::Up(session),
            None if self == Self::Connecting => Models::Down(GitHub::Connecting),
            None => Models::Down(GitHub::Disconnected),
        }
    }
}

/// Whether a visible run's `Done` settles the run focused history shows.
pub fn settles(runs: &Runs, run: RunId, phase: Phase, applied: &Applied) -> bool {
    phase == Phase::Done
        && applied.visible
        && runs
            .shown()
            .is_some_and(|shown| shown.run == run && shown.done)
}

/// A run's `Focus` moves the typed input only from the current pair, and only while the runs
/// agree.
pub fn focus(
    runs: &mut Runs,
    current: Option<u64>,
    (generation, run, thread): (u64, RunId, ThreadId),
) -> bool {
    current == Some(generation) && runs.focus(run, thread)
}

/// A scene frame's run disposition: a current pair's step commit for a stopped run is
/// `Stopped`. Stale frames still reach the presenter, which drops them off the UI thread.
pub fn scene_disposition(
    runs: &Runs,
    current: Option<u64>,
    generation: u64,
    frame: &DataToShell,
) -> RunDisposition {
    match frame {
        DataToShell::SceneStepCommit { run, .. } if current == Some(generation) => {
            runs.owned_disposition(*run)
        }
        _ => RunDisposition::Live,
    }
}

/// The credit a current pair's removed or replaced scenes free in the runs.
pub fn removed_scenes(
    runs: &mut Runs,
    current: Option<u64>,
    generation: u64,
    frame: &DataToShell,
) -> u32 {
    match frame {
        DataToShell::ScenesRemoved { scenes } | DataToShell::ScenesReplaced { scenes, .. }
            if current == Some(generation) =>
        {
            runs.remove_scenes(scenes)
        }
        _ => 0,
    }
}

/// Whether the brain was told about a speaker stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Told {
    /// The brain took a `Stop`; a busy owner that never takes it ends the pair.
    Brain,
    /// A terminal run's audible tail: only its speaker stops, and nothing ends the pair.
    SpeakerOnly,
}

struct Pending<S> {
    session: S,
    run: RunId,
    tries: u8,
    told: Told,
}

/// Speaker stops a busy owner hasn't taken yet, oldest first, each with its tries left and the
/// pair `S` it was made on.
pub struct SpeakerStops<S>(VecDeque<Pending<S>>);

impl<S> Default for SpeakerStops<S> {
    fn default() -> Self {
        Self(VecDeque::new())
    }
}

/// What follows the owner's answer to the front stop.
#[derive(PartialEq, Eq)]
pub enum Offered<'a, S> {
    /// The stop is done with; offer the next one now.
    Next,
    /// Offer it again after [`STOP_RETRY`], with later stops behind it. A speaker-only stop
    /// first tells its pair's queued worker to stop that run's playback.
    Retry { playback: Option<(&'a S, RunId)> },
    /// A brain-told stop ran out of tries or its owner is lost: end that pair, then go on.
    End(&'a S),
}

impl<S> fmt::Debug for Offered<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Next => "Next",
            Self::Retry { playback: Some(_) } => "RetryPlayback",
            Self::Retry { playback: None } => "Retry",
            Self::End(_) => "End",
        })
    }
}

impl<S> SpeakerStops<S> {
    pub fn push(&mut self, session: S, run: RunId, told: Told) {
        self.0.push_back(Pending {
            session,
            run,
            tries: STOP_TRIES,
            told,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The stop to offer next.
    pub fn front(&self) -> Option<(&S, RunId, Told)> {
        self.0
            .front()
            .map(|pending| (&pending.session, pending.run, pending.told))
    }

    /// Takes the owner's answer to the front stop. `audio` is read only for a lost owner of a
    /// brain-told stop.
    pub fn offered(
        &mut self,
        stopped: Result<(), Denied>,
        audio: impl FnOnce() -> AudioSession,
    ) -> Offered<'_, S> {
        let Some(front) = self.0.front_mut() else {
            return Offered::Next;
        };
        let local = front.told == Told::SpeakerOnly;
        let retry = stopped == Err(Denied::Busy) && front.tries > 1;
        let end = !retry
            && !local
            && match stopped {
                Err(Denied::Busy) => true,
                Err(Denied::Lost) => audio() == AudioSession::QueuedRequired,
                _ => false,
            };
        if retry {
            front.tries -= 1;
        } else if end {
            front.tries = 0;
        } else {
            // Queued, or its session is gone or ending already, or a speaker-only stop refused.
            self.0.pop_front();
            return Offered::Next;
        }
        self.0.front().map_or(Offered::Next, |front| {
            if end {
                Offered::End(&front.session)
            } else {
                Offered::Retry {
                    playback: local.then_some((&front.session, front.run)),
                }
            }
        })
    }

    /// After a retry's wait or a pair's end: a front stop with no tries left is dropped.
    pub fn resume(&mut self) {
        if self.0.front().is_some_and(|pending| pending.tries == 0) {
            self.0.pop_front();
        }
    }
}

/// What follows queueing a stop or scene control for the brain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The brain was told; that run's speaker stop follows in order.
    StopSpeaker(RunId),
    /// The brain was told a control that names no run.
    Told,
    /// The brain wasn't told on an open pair: the run's queued playback stops first, then the
    /// pair ends, since a control the brain never got can't be undone locally.
    Seal(Option<RunId>),
    /// The brain wasn't told, and the pair is ending already.
    Refused,
}

impl Sent {
    /// Sends `message` first, then decides what follows. `open` is read only after a refusal.
    pub fn send(
        message: ControlToBrain,
        send: impl FnOnce(ControlToBrain) -> bool,
        open: impl FnOnce() -> bool,
    ) -> Self {
        let run = match &message {
            ControlToBrain::Stop { run, .. } | ControlToBrain::StopScenes { run, .. } => Some(*run),
            _ => None,
        };
        if send(message) {
            run.map_or(Self::Told, Self::StopSpeaker)
        } else if open() {
            Self::Seal(run)
        } else {
            Self::Refused
        }
    }

    pub fn told(self) -> bool {
        matches!(self, Self::StopSpeaker(_) | Self::Told)
    }
}

/// A refused or cancelled run's stop, resolved at the point where it is applied.
pub enum RunStop {
    Hold { run: RunId, revealed: Vec<u32> },
    Ordinary(ControlToBrain),
}

impl RunStop {
    pub fn owed(origin: Option<RunOrigin>, message: ControlToBrain) -> Self {
        match (origin, message) {
            (Some(RunOrigin::Hold { .. }), ControlToBrain::Stop { run, revealed }) => {
                Self::Hold { run, revealed }
            }
            (_, message) => Self::Ordinary(message),
        }
    }
}

/// A hold run's owed stop: its scenes' `StopScenes` when that cuts anything, else the run's
/// `Stop` in its place, so the brain hears one of them.
pub fn hold_run_stop(
    presenter: &mut Presenter,
    run: RunId,
    revealed: Vec<u32>,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    presenter.stop_owned(run, now).map(|mut effects| {
        let cut = effects.iter().any(|effect| match effect {
            Effect::Control(ControlToBrain::StopScenes { cuts, .. }) => !cuts.is_empty(),
            _ => false,
        });
        if !cut {
            effects.retain(|effect| {
                !matches!(effect, Effect::Control(ControlToBrain::StopScenes { .. }))
            });
            effects.push(Effect::Control(ControlToBrain::Stop { run, revealed }));
        }
        effects
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use kumochi_protocol::message::{
        AudioSession, ControlToBrain, DataToShell, GestureId, Incarnation, Phase, RunId, RunOrigin,
        SceneId, ThreadId,
    };

    use super::{
        GitHub, Lead, Models, Offered, RunStop, STOP_TRIES, Sent, SpeakerStops, Told, focus,
        scene_disposition, settles,
    };
    use crate::link::output::Denied;
    use crate::link::{LinkEvent, LinkState};
    use crate::runs::{Applied, RunDisposition, Runs};

    #[test]
    fn a_link_state_leads_and_brings_the_models_up_only_on_a_connected_pair() {
        let pair = 1_u64;
        assert_eq!(
            LinkState::Connected.lead(),
            Lead {
                clears_refusal: true,
                retry: false
            }
        );
        assert_eq!(
            LinkState::Failed.lead(),
            Lead {
                clears_refusal: false,
                retry: true
            }
        );
        for state in [
            LinkState::Connecting,
            LinkState::Reconnecting,
            LinkState::OutputUnavailable,
        ] {
            assert_eq!(
                state.lead(),
                Lead {
                    clears_refusal: false,
                    retry: false
                },
                "{state:?}"
            );
        }
        assert_eq!(LinkState::Connected.models(Some(&pair)), Models::Up(&pair));
        assert_eq!(
            LinkState::Connected.models(None::<&u64>),
            Models::Down(GitHub::Disconnected)
        );
        assert_eq!(
            LinkState::Connecting.models(Some(&pair)),
            Models::Down(GitHub::Connecting)
        );
        for state in [
            LinkState::Reconnecting,
            LinkState::Failed,
            LinkState::OutputUnavailable,
        ] {
            assert_eq!(
                state.models(Some(&pair)),
                Models::Down(GitHub::Disconnected),
                "{state:?}"
            );
        }
    }

    #[test]
    fn only_a_visible_done_of_the_shown_run_settles_focused_history() {
        let mut runs = Runs::default();
        runs.settings(ThreadId(1));
        runs.apply_state(RUN, ThreadId(1), Phase::Answering, None);
        runs.apply_data(
            DataToShell::AssistantText {
                run: RUN,
                delta: "kept".into(),
            },
            0,
        );
        let (answering, _) = runs.apply_run_state(RUN, ThreadId(1), Phase::Answering, None, None);
        assert!(!settles(&runs, RUN, Phase::Answering, &answering));
        let (done, _) = runs.apply_run_state(RUN, ThreadId(1), Phase::Done, None, None);
        assert!(settles(&runs, RUN, Phase::Done, &done));
        assert!(!settles(&runs, RunId(6), Phase::Done, &done), "another run");
        let fenced = Applied {
            visible: false,
            ..done
        };
        assert!(
            !settles(&runs, RUN, Phase::Done, &fenced),
            "Esc or a delete fenced it"
        );
    }

    #[test]
    fn a_focus_moves_the_input_only_from_the_current_pair() {
        let mut runs = Runs::default();
        runs.met(Incarnation(1));
        runs.connected(1);
        runs.settings(ThreadId(1));
        runs.submit("go there", |_| true).expect("sent");
        runs.apply_state(
            RUN,
            ThreadId(1),
            Phase::Thinking,
            Some(RunOrigin::Typed { turn: 1 }),
        );
        let task = ThreadId(9);
        assert!(!focus(&mut runs, Some(2), (1, RUN, task)), "another pair");
        assert!(!focus(&mut runs, None, (1, RUN, task)), "no pair");
        assert!(focus(&mut runs, Some(1), (1, RUN, task)));
    }

    #[test]
    fn a_current_pairs_step_for_a_stopped_run_is_stopped_and_any_other_frame_live() {
        let mut runs = Runs::default();
        runs.apply_state(RUN, ThreadId(1), Phase::Answering, None);
        assert!(runs.fence_run(RUN).is_some());
        let commit = |run| DataToShell::SceneStepCommit {
            run,
            scene: SceneId(3),
            index: 0,
            step: crate::presenter::fixture::pair(),
            speech: None,
        };
        assert_eq!(
            scene_disposition(&runs, Some(1), 1, &commit(RUN)),
            RunDisposition::Stopped
        );
        assert_eq!(
            scene_disposition(&runs, Some(2), 1, &commit(RUN)),
            RunDisposition::Live,
            "another pair's frame"
        );
        assert_eq!(
            scene_disposition(&runs, Some(1), 1, &commit(RunId(6))),
            RunDisposition::Live
        );
        assert_eq!(
            scene_disposition(
                &runs,
                Some(1),
                1,
                &DataToShell::ScenesRemoved { scenes: Vec::new() }
            ),
            RunDisposition::Live
        );
    }

    const RUN: RunId = RunId(5);

    #[test]
    fn link_decisions_never_format_the_pair() {
        let marker = "pair-payload-sentinel";
        let mut stops = SpeakerStops::default();
        stops.push(marker, RUN, Told::SpeakerOnly);
        let retry = stops.offered(Err(Denied::Busy), no_audio);
        let end = Offered::End(&marker);
        let up = LinkState::Connected.models(Some(&marker));
        for output in [format!("{retry:?}"), format!("{end:?}"), format!("{up:?}")] {
            assert!(!output.contains(marker));
            assert!(!output.contains(&format!("{:?}", marker.as_bytes())));
        }
    }

    #[test]
    fn only_a_hold_run_stop_uses_its_own_scene_stop_path() {
        let stop = || ControlToBrain::Stop {
            run: RUN,
            revealed: vec![2],
        };
        let hold = Some(RunOrigin::Hold {
            gesture: GestureId(1),
        });
        let RunStop::Hold { run, revealed } = RunStop::owed(hold, stop()) else {
            panic!("a Hold must not stop a newer drawing");
        };
        assert_eq!((run, revealed), (RUN, vec![2]));
        let typed = Some(RunOrigin::Typed { turn: 1 });
        let RunStop::Ordinary(message) = RunStop::owed(typed, stop()) else {
            panic!("a typed stop uses the ordinary path");
        };
        assert_eq!(message, stop());
        assert!(matches!(
            RunStop::owed(hold, ControlToBrain::Pong),
            RunStop::Ordinary(ControlToBrain::Pong)
        ));
    }

    fn no_audio() -> AudioSession {
        unreachable!("only a lost owner of a brain-told stop reads the audio session")
    }

    #[test]
    fn only_pair_named_events_from_another_pair_are_stale() {
        let refused = LinkEvent::TurnRefused {
            generation: 1,
            turn: 2,
            thread: None,
        };
        assert!(!refused.stale(Some(1)));
        assert!(refused.stale(Some(2)));
        assert!(refused.stale(None), "no pair is current");
        assert!(!LinkEvent::ThreadsChanged.stale(None));
    }

    #[test]
    fn a_busy_owner_ends_the_pair_only_after_a_brain_told_stop_runs_out_of_tries() {
        let mut stops = SpeakerStops::default();
        stops.push(1_u64, RUN, Told::Brain);
        stops.push(2, RunId(6), Told::Brain);
        for _ in 1..STOP_TRIES {
            assert_eq!(
                stops.offered(Err(Denied::Busy), no_audio),
                Offered::Retry { playback: None }
            );
            stops.resume();
            assert_eq!(stops.front(), Some((&1, RUN, Told::Brain)), "still first");
        }
        assert_eq!(stops.offered(Err(Denied::Busy), no_audio), Offered::End(&1));
        assert_eq!(
            stops.front(),
            Some((&1, RUN, Told::Brain)),
            "until it ended"
        );
        stops.resume();
        assert_eq!(stops.front(), Some((&2, RunId(6), Told::Brain)));
        assert_eq!(stops.offered(Ok(()), no_audio), Offered::Next);
        assert!(stops.is_empty());
        stops.resume();
        assert!(stops.is_empty());
    }

    #[test]
    fn a_speaker_only_stop_retries_its_playback_and_never_ends_the_pair() {
        let mut stops = SpeakerStops::default();
        stops.push(1_u64, RUN, Told::SpeakerOnly);
        for _ in 1..STOP_TRIES {
            assert_eq!(
                stops.offered(Err(Denied::Busy), no_audio),
                Offered::Retry {
                    playback: Some((&1, RUN))
                }
            );
        }
        assert_eq!(stops.offered(Err(Denied::Busy), no_audio), Offered::Next);
        assert!(stops.is_empty(), "dropped without ending its pair");
        stops.push(1, RUN, Told::SpeakerOnly);
        assert_eq!(stops.offered(Err(Denied::Lost), no_audio), Offered::Next);
        assert!(stops.is_empty());
    }

    #[test]
    fn a_lost_owner_ends_the_pair_only_for_required_queued_audio() {
        let mut stops = SpeakerStops::default();
        stops.push(1_u64, RUN, Told::Brain);
        assert_eq!(
            stops.offered(Err(Denied::Lost), || AudioSession::Legacy),
            Offered::Next
        );
        assert!(stops.is_empty());
        stops.push(1, RUN, Told::Brain);
        assert_eq!(
            stops.offered(Err(Denied::Lost), || AudioSession::QueuedRequired),
            Offered::End(&1)
        );
        stops.resume();
        assert!(stops.is_empty());
        stops.push(1, RUN, Told::Brain);
        assert_eq!(stops.offered(Err(Denied::Stale), no_audio), Offered::Next);
        assert!(stops.is_empty());
        assert_eq!(
            stops.offered(Ok(()), no_audio),
            Offered::Next,
            "nothing queued"
        );
    }

    #[test]
    fn a_stop_is_sent_first_and_a_refusal_seals_only_an_open_pair() {
        let order = &RefCell::new(Vec::new());
        let send = |told| {
            move |message: ControlToBrain| {
                order.borrow_mut().push("send");
                assert!(matches!(message, ControlToBrain::Stop { run: RUN, .. }));
                told
            }
        };
        let open = |open| {
            move || {
                order.borrow_mut().push("open");
                open
            }
        };
        let stop = || ControlToBrain::Stop {
            run: RUN,
            revealed: vec![1],
        };
        assert_eq!(
            Sent::send(stop(), send(false), open(true)),
            Sent::Seal(Some(RUN))
        );
        assert_eq!(*order.borrow(), ["send", "open"]);
        assert_eq!(Sent::send(stop(), send(false), open(false)), Sent::Refused);
        order.borrow_mut().clear();
        let told = Sent::send(stop(), send(true), open(true));
        assert_eq!(told, Sent::StopSpeaker(RUN));
        assert_eq!(*order.borrow(), ["send"], "an accepted send reads no pair");
        assert!(told.told());
        let pong = Sent::send(ControlToBrain::Pong, |_| true, || unreachable!());
        assert_eq!(pong, Sent::Told);
        assert!(pong.told());
        assert!(!Sent::Seal(None).told());
        assert!(!Sent::Refused.told());
    }
}
