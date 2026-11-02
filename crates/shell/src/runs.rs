//! The runs as the shell shows them: which one is moving, which turns Esc has cancelled before
//! their run was named, which runs it has fenced off, and the placeholder view of the newest one.
//! Plain state, no GPUI, so every rule is unit-tested.
//!
//! The fence is checked when a message is applied, not when it's received (architecture v1
//! §3.3), so messages already queued in the shell when Esc fences their run are dropped too.
//! Data for a run no `RunState` has named yet is held, with its credit, until one does, so a run
//! Esc cancelled before it was named never shows anything.

use capsula_protocol::frame::Pipe;
use capsula_protocol::message::{
    ControlToBrain, DataToBrain, DataToShell, MAX_PENDING_TURNS, MAX_STEPS, Phase, RunId, Step,
    ThreadId,
};

/// The most answer text the placeholder keeps, from the end [guess].
const TEXT_KEPT: usize = 4096;
/// Room in a data frame for a `TextSubmit`'s other fields [guess].
const SUBMIT_OVERHEAD: usize = 64;

/// Why a turn wasn't sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Nothing but whitespace: the brain ignores it, so it would never be answered.
    Blank,
    /// Longer than a data frame can carry.
    TooLong,
    /// So many turns are unanswered that one more could make the brain end the connection.
    Busy,
    /// No brain to send it to.
    Offline,
}

/// The newest run on screen.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Shown {
    pub(crate) run: RunId,
    /// The end of its answer, at most [`TEXT_KEPT`] bytes.
    pub(crate) text: String,
    /// Its committed steps, by index.
    pub(crate) steps: Vec<(u32, Step)>,
}

/// What applying a `RunState` asks of the caller.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// A `Stop` for a run Esc cancelled before it was named.
    pub(crate) stop: Option<ControlToBrain>,
    /// Credit for held data frames this state classified.
    pub(crate) credit: u32,
}

/// The newest named run, until both its terminal `RunState` and its `RunEnd` have been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Live {
    run: RunId,
    state_ended: bool,
    data_ended: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Runs {
    connected: bool,
    /// The newest named run while its output may still arrive.
    live: Option<Live>,
    /// The highest run Esc stopped; anything of it or older is dropped when applied.
    fence: RunId,
    /// The highest run whose drawing Esc cleared; its steps are dropped when applied.
    cleared: RunId,
    /// The highest run any `RunState` named: each new one answers the oldest pending turn.
    answered: RunId,
    /// Turns sent and not answered yet.
    pending: usize,
    /// How many of the oldest pending turns Esc cancelled; never more than `pending`.
    cancelled: usize,
    /// Data frames, with their size, for runs not named yet. Their credit is withheld, so the
    /// brain's credit window bounds them.
    held: Vec<(DataToShell, u32)>,
    /// The thread the next turn continues.
    thread: Option<ThreadId>,
    /// Turns sent before the latest explicit selection must not replace that choice.
    selection_pending: usize,
    shown: Option<Shown>,
}

impl Runs {
    /// Selects the thread the next typed turn continues; no message or store write is needed.
    pub(crate) fn select_thread(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.selection_pending = self.pending;
    }

    /// A run is moving that Esc hasn't stopped, or a turn is waiting for its run.
    pub(crate) fn working(&self) -> bool {
        self.stoppable().is_some() || self.pending > self.cancelled
    }

    /// Steps are on screen.
    pub(crate) fn drawing(&self) -> bool {
        self.shown
            .as_ref()
            .is_some_and(|shown| !shown.steps.is_empty())
    }

    pub(crate) fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
    }

    fn stoppable(&self) -> Option<RunId> {
        self.live
            .map(|live| live.run)
            .filter(|run| *run > self.fence)
    }

    /// The first Esc: cancels every turn sent and not yet answered, fences the moving run off,
    /// keeping what's on screen (a placeholder step has no entrance, so every one shown has
    /// finished entering), and returns the `Stop` to send after. A cancelled turn's `Stop`
    /// comes from [`Runs::apply_state`] once its run is named.
    pub(crate) fn stop(&mut self) -> Option<ControlToBrain> {
        self.cancelled = self.pending;
        let run = self.stoppable()?;
        self.fence = run;
        let revealed = self
            .shown
            .as_ref()
            .filter(|shown| shown.run == run)
            .map(|shown| shown.steps.iter().map(|(index, _)| *index).collect())
            .unwrap_or_default();
        Some(ControlToBrain::Stop { run, revealed })
    }

    /// The second Esc, or Esc while nothing moves: clears the drawing and returns the
    /// `ClearDrawing` to send after. The text stays.
    pub(crate) fn clear(&mut self) -> Option<ControlToBrain> {
        let shown = self
            .shown
            .as_mut()
            .filter(|shown| !shown.steps.is_empty())?;
        shown.steps.clear();
        self.cleared = self.cleared.max(shown.run);
        Some(ControlToBrain::ClearDrawing { run: shown.run })
    }

    /// Sends a typed turn through `send`, which says whether it went, unless it would be refused.
    ///
    /// # Errors
    /// [`Refused`] says why it wasn't sent.
    pub(crate) fn submit(
        &mut self,
        text: &str,
        send: impl FnOnce(DataToBrain) -> bool,
    ) -> Result<(), Refused> {
        if text.trim().is_empty() {
            return Err(Refused::Blank);
        }
        if text.len() > Pipe::Data.max_payload() - SUBMIT_OVERHEAD {
            return Err(Refused::TooLong);
        }
        if !self.connected {
            return Err(Refused::Offline);
        }
        if self.pending + 1 >= MAX_PENDING_TURNS {
            return Err(Refused::Busy);
        }
        let turn = DataToBrain::TextSubmit {
            thread: self.thread,
            text: text.to_owned(),
        };
        if !send(turn) {
            return Err(Refused::Offline);
        }
        self.pending += 1;
        Ok(())
    }

    /// The link connected.
    pub(crate) fn connected(&mut self) {
        self.connected = true;
    }

    /// The link lost the brain: nothing of the runs it knew can arrive any more, and the turns
    /// it hadn't answered never will be.
    pub(crate) fn disconnected(&mut self) {
        self.connected = false;
        self.fence = self.fence.max(self.answered);
        self.live = None;
        self.pending = 0;
        self.selection_pending = 0;
        self.cancelled = 0;
        self.held.clear();
    }

    /// A `RunState`. A run newer than any before answers the oldest pending turn exactly once,
    /// fenced or not, and is fenced before any of its output is applied if Esc cancelled that
    /// turn. Only such a run sets the next thread, and never over a later explicit selection.
    pub(crate) fn apply_state(&mut self, run: RunId, thread: ThreadId, phase: Phase) -> Applied {
        let mut applied = Applied::default();
        if run > self.answered {
            self.answered = run;
            if self.selection_pending == 0 {
                self.thread = Some(thread);
            } else {
                self.selection_pending -= 1;
            }
            if self.pending > 0 {
                self.pending -= 1;
                if self.cancelled > 0 {
                    self.cancelled -= 1;
                    self.fence = self.fence.max(run);
                    applied.stop = Some(ControlToBrain::Stop {
                        run,
                        revealed: Vec::new(),
                    });
                }
            }
            if run > self.fence {
                self.live = Some(Live {
                    run,
                    state_ended: false,
                    data_ended: false,
                });
            }
        }
        if matches!(phase, Phase::Done | Phase::Interrupted | Phase::Failed) {
            self.end(run, |live| live.state_ended = true);
        }
        applied.credit = self.release();
        applied
    }

    /// A data frame of `bytes` on the pipe. Returns the credit to grant for it now: none while
    /// it's held for its run's first `RunState`.
    pub(crate) fn apply_data(&mut self, message: DataToShell, bytes: u32) -> u32 {
        if run_of(&message).is_some_and(|run| run > self.answered) {
            self.held.push((message, bytes));
            return 0;
        }
        self.show(message);
        bytes
    }

    /// Applies the held frames whose run is named now, in arrival order, and returns their credit.
    fn release(&mut self) -> u32 {
        let answered = self.answered;
        let (ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(message, _)| run_of(message).is_none_or(|run| run <= answered));
        self.held = held;
        ready.into_iter().fold(0, |credit, (message, bytes)| {
            self.show(message);
            credit.saturating_add(bytes)
        })
    }

    /// A newer run takes over the view, and anything fenced, cleared or older than the view is
    /// dropped.
    fn show(&mut self, message: DataToShell) {
        match message {
            DataToShell::AssistantText { run, delta } => {
                if let Some(shown) = self.view(run) {
                    shown.text.push_str(&delta);
                    keep_end(&mut shown.text, TEXT_KEPT);
                }
            }
            DataToShell::StepCommit { run, index, step } => {
                if run <= self.cleared {
                    return;
                }
                if let Some(shown) = self.view(run)
                    && shown.steps.len() < MAX_STEPS
                {
                    shown.steps.push((index, step));
                }
            }
            DataToShell::RunEnd { run } => self.end(run, |live| live.data_ended = true),
            DataToShell::ThreadList { .. }
            | DataToShell::Page { .. }
            | DataToShell::PageFailed { .. } => {}
        }
    }

    /// Marks one of `run`'s two endings; the live run settles once both are in, and an older
    /// run's ending never touches a newer one.
    fn end(&mut self, run: RunId, mark: impl FnOnce(&mut Live)) {
        if let Some(live) = self.live.as_mut().filter(|live| live.run == run) {
            mark(live);
            if live.state_ended && live.data_ended {
                self.live = None;
            }
        }
    }

    /// The view `run`'s data goes to, if it isn't fenced or older than the view.
    fn view(&mut self, run: RunId) -> Option<&mut Shown> {
        if run <= self.fence || self.shown.as_ref().is_some_and(|shown| run < shown.run) {
            return None;
        }
        if self.shown.as_ref().is_none_or(|shown| run > shown.run) {
            self.shown = Some(Shown {
                run,
                ..Shown::default()
            });
        }
        self.shown.as_mut()
    }
}

fn run_of(message: &DataToShell) -> Option<RunId> {
    match message {
        DataToShell::AssistantText { run, .. }
        | DataToShell::StepCommit { run, .. }
        | DataToShell::RunEnd { run } => Some(*run),
        DataToShell::ThreadList { .. }
        | DataToShell::Page { .. }
        | DataToShell::PageFailed { .. } => None,
    }
}

/// Drops the start of `text` so at most `max` bytes stay, cutting at a character boundary.
fn keep_end(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
}

#[cfg(test)]
mod tests {
    use capsula_protocol::frame::Pipe;
    use capsula_protocol::message::{
        ControlToBrain, DataToBrain, DataToShell, MAX_PENDING_TURNS, Phase, RunId, Step, ThreadId,
    };

    use super::{Applied, Refused, Runs, TEXT_KEPT};
    use crate::escape::{Activity, Escape, Panel, decide};

    /// Every data frame in these tests is this size on the pipe.
    const BYTES: u32 = 10;

    #[test]
    fn selecting_and_starting_a_new_thread_only_change_the_next_turn() {
        let mut runs = answering(5);
        runs.select_thread(Some(ThreadId(9)));
        runs.apply_state(RunId(5), ThreadId(1), Phase::Answering);
        let mut sent = Vec::new();
        runs.submit("continue", |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
        runs.select_thread(None);
        runs.submit("new", |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
        assert_eq!(
            sent,
            [
                DataToBrain::TextSubmit {
                    thread: Some(ThreadId(9)),
                    text: "continue".to_owned()
                },
                DataToBrain::TextSubmit {
                    thread: None,
                    text: "new".to_owned()
                },
            ]
        );
        assert!(runs.working(), "selection didn't stop work");
    }

    #[test]
    fn pages_are_never_held_for_a_run_state() {
        let mut runs = Runs::default();
        let page = DataToShell::Page {
            request: 1,
            thread: ThreadId(999),
            events: Vec::new(),
            before: None,
        };
        assert_eq!(runs.apply_data(page, BYTES), BYTES);
        assert!(runs.held.is_empty());
        assert!(runs.shown().is_none());
    }

    #[test]
    fn an_unanswered_old_turn_cannot_override_a_new_thread_choice() {
        let mut runs = Runs::default();
        runs.connected();
        submit(&mut runs, "old pending turn");
        runs.select_thread(None);
        runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
        let mut sent = Vec::new();
        runs.submit("new thread turn", |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
        runs.apply_state(RunId(2), ThreadId(22), Phase::Thinking);
        runs.submit("continue new thread", |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
        assert_eq!(
            sent,
            [
                DataToBrain::TextSubmit {
                    thread: None,
                    text: "new thread turn".to_owned()
                },
                DataToBrain::TextSubmit {
                    thread: Some(ThreadId(22)),
                    text: "continue new thread".to_owned()
                },
            ]
        );
    }

    fn text(run: u64, delta: &str) -> DataToShell {
        DataToShell::AssistantText {
            run: RunId(run),
            delta: delta.to_owned(),
        }
    }

    fn step(run: u64, index: u32) -> DataToShell {
        DataToShell::StepCommit {
            run: RunId(run),
            index,
            step: Step {
                label: format!("step {index}"),
            },
        }
    }

    fn end(run: u64) -> DataToShell {
        DataToShell::RunEnd { run: RunId(run) }
    }

    fn stop(run: u64) -> ControlToBrain {
        ControlToBrain::Stop {
            run: RunId(run),
            revealed: Vec::new(),
        }
    }

    fn state(runs: &mut Runs, run: u64, phase: Phase) -> Applied {
        runs.apply_state(RunId(run), ThreadId(1), phase)
    }

    fn data(runs: &mut Runs, message: DataToShell) -> u32 {
        runs.apply_data(message, BYTES)
    }

    fn submit(runs: &mut Runs, typed: &str) {
        runs.submit(typed, |_| true).expect("sent");
    }

    /// Connected, with `run` answering in thread 1.
    fn answering(run: u64) -> Runs {
        let mut runs = Runs::default();
        runs.connected();
        state(&mut runs, run, Phase::Answering);
        runs
    }

    fn indices(runs: &Runs) -> Vec<u32> {
        runs.shown()
            .map(|shown| shown.steps.iter().map(|(index, _)| *index).collect())
            .unwrap_or_default()
    }

    fn shown_text(runs: &Runs) -> Option<(RunId, &str)> {
        runs.shown().map(|shown| (shown.run, shown.text.as_str()))
    }

    #[test]
    fn stop_fences_the_run_and_reveals_what_is_on_screen() {
        let mut runs = answering(5);
        data(&mut runs, step(5, 0));
        data(&mut runs, step(5, 1));
        assert!(runs.working());
        assert_eq!(
            runs.stop(),
            Some(ControlToBrain::Stop {
                run: RunId(5),
                revealed: vec![0, 1],
            })
        );
        assert!(!runs.working());
        assert_eq!(runs.stop(), None, "nothing else moves");
        assert!(runs.drawing(), "the steps stay");
    }

    #[test]
    fn a_fenced_run_s_messages_are_dropped_when_applied() {
        let mut runs = answering(5);
        data(&mut runs, text(5, "kept "));
        runs.stop();
        // Already queued in the shell when Esc came.
        assert_eq!(
            data(&mut runs, text(5, "late")),
            BYTES,
            "dropped, still credited"
        );
        data(&mut runs, step(5, 0));
        state(&mut runs, 5, Phase::Answering);
        assert!(!runs.working());
        let shown = runs.shown().expect("shown");
        assert_eq!(shown.text, "kept ");
        assert!(shown.steps.is_empty());
    }

    #[test]
    fn clear_drops_the_drawing_and_keeps_late_steps_out() {
        let mut runs = answering(5);
        data(&mut runs, text(5, "words"));
        data(&mut runs, step(5, 0));
        state(&mut runs, 5, Phase::Done);
        data(&mut runs, end(5));
        assert!(!runs.working());
        assert_eq!(
            runs.clear(),
            Some(ControlToBrain::ClearDrawing { run: RunId(5) })
        );
        assert!(!runs.drawing());
        assert_eq!(runs.clear(), None, "nothing left to clear");
        data(&mut runs, step(5, 1));
        assert_eq!(indices(&runs), Vec::<u32>::new());
        assert_eq!(
            shown_text(&runs),
            Some((RunId(5), "words")),
            "the text stays"
        );
        state(&mut runs, 6, Phase::Thinking);
        data(&mut runs, step(6, 0));
        assert_eq!(indices(&runs), vec![0], "a newer run draws again");
    }

    #[test]
    fn a_newer_run_takes_over_the_view_and_older_data_is_dropped() {
        let mut runs = answering(5);
        data(&mut runs, text(5, "old"));
        state(&mut runs, 6, Phase::Thinking);
        data(&mut runs, text(6, "new"));
        data(&mut runs, text(5, " late"));
        assert_eq!(shown_text(&runs), Some((RunId(6), "new")));
    }

    #[test]
    fn data_before_its_run_state_is_held_until_the_state_names_it() {
        let mut runs = Runs::default();
        runs.connected();
        submit(&mut runs, "hi");
        assert_eq!(
            data(&mut runs, text(3, "early")),
            0,
            "held, credit withheld"
        );
        assert!(runs.shown().is_none());
        let applied = state(&mut runs, 3, Phase::Answering);
        assert_eq!(
            applied,
            Applied {
                stop: None,
                credit: BYTES
            }
        );
        assert_eq!(shown_text(&runs), Some((RunId(3), "early")));
        assert!(runs.working());
    }

    #[test]
    fn esc_cancels_a_turn_whose_run_isnt_named_yet() {
        let mut runs = Runs::default();
        runs.connected();
        submit(&mut runs, "a");
        assert!(runs.working(), "a sent turn is work");
        assert_eq!(runs.stop(), None, "nothing to name yet");
        assert!(!runs.working());
        submit(&mut runs, "b");
        assert!(runs.working(), "a turn after Esc isn't cancelled");
        assert_eq!(data(&mut runs, text(1, "a's early text")), 0);
        let applied = state(&mut runs, 1, Phase::Thinking);
        assert_eq!(
            applied,
            Applied {
                stop: Some(stop(1)),
                credit: BYTES
            }
        );
        assert!(runs.shown().is_none(), "nothing of a shows");
        assert_eq!(runs.pending, 1, "a used exactly one slot");
        data(&mut runs, text(1, "more"));
        data(&mut runs, step(1, 0));
        assert_eq!(state(&mut runs, 1, Phase::Interrupted), Applied::default());
        assert!(runs.shown().is_none());
        assert_eq!(state(&mut runs, 2, Phase::Thinking), Applied::default());
        data(&mut runs, text(2, "b runs"));
        assert_eq!(shown_text(&runs), Some((RunId(2), "b runs")));
        assert!(runs.working());
        assert_eq!(runs.pending, 0);
    }

    #[test]
    fn esc_stops_the_known_run_and_cancels_the_turn_queued_behind_it() {
        let mut runs = Runs::default();
        runs.connected();
        submit(&mut runs, "first");
        state(&mut runs, 4, Phase::Answering);
        data(&mut runs, text(4, "shown"));
        submit(&mut runs, "a");
        assert_eq!(runs.stop(), Some(stop(4)));
        submit(&mut runs, "b");
        assert_eq!(data(&mut runs, text(5, "a's text")), 0);
        assert_eq!(
            state(&mut runs, 5, Phase::Thinking),
            Applied {
                stop: Some(stop(5)),
                credit: BYTES
            },
            "exactly one Stop names a's run"
        );
        assert_eq!(state(&mut runs, 5, Phase::Answering), Applied::default());
        assert_eq!(shown_text(&runs), Some((RunId(4), "shown")));
        assert_eq!(runs.pending, 1);
        state(&mut runs, 6, Phase::Thinking);
        data(&mut runs, text(6, "b"));
        assert_eq!(shown_text(&runs), Some((RunId(6), "b")));
        assert!(runs.working());
    }

    #[test]
    fn a_run_stays_stoppable_until_its_data_has_drained() {
        let mut runs = answering(5);
        data(&mut runs, text(5, "seen"));
        state(&mut runs, 5, Phase::Done);
        assert!(runs.working(), "its data hasn't ended");
        assert_eq!(
            runs.stop(),
            Some(ControlToBrain::Stop {
                run: RunId(5),
                revealed: Vec::new(),
            })
        );
        data(&mut runs, text(5, " late"));
        data(&mut runs, step(5, 0));
        data(&mut runs, end(5));
        assert_eq!(shown_text(&runs), Some((RunId(5), "seen")));
        assert!(!runs.drawing());
        assert!(!runs.working());
    }

    #[test]
    fn a_run_settles_whichever_ending_arrives_first() {
        let mut runs = answering(5);
        data(&mut runs, end(5));
        assert!(runs.working(), "its state hasn't ended");
        state(&mut runs, 5, Phase::Done);
        assert!(!runs.working());
        assert_eq!(runs.stop(), None);
    }

    #[test]
    fn a_terminal_state_first_waits_for_run_end_to_settle() {
        let mut runs = answering(5);
        state(&mut runs, 5, Phase::Done);
        assert!(runs.working(), "its data hasn't ended");
        data(&mut runs, end(5));
        assert!(!runs.working());
        assert_eq!(runs.stop(), None);
    }

    #[test]
    fn a_reconnect_forgets_turns_esc_cancelled_and_their_held_data() {
        let mut runs = Runs::default();
        runs.connected();
        submit(&mut runs, "a");
        assert_eq!(runs.stop(), None, "a isn't named yet");
        assert_eq!(data(&mut runs, text(1, "a's text")), 0, "held");
        runs.disconnected();
        runs.connected();
        submit(&mut runs, "b");
        assert_eq!(
            state(&mut runs, 2, Phase::Answering),
            Applied {
                stop: None,
                credit: 0
            },
            "b isn't cancelled, and a's held frame is gone"
        );
        assert_eq!(data(&mut runs, text(2, "b's answer")), BYTES);
        assert_eq!(shown_text(&runs), Some((RunId(2), "b's answer")));
        assert!(runs.working());
    }

    /// One Esc press, decided the way the presence decides it with the typing panel open.
    fn press(runs: &mut Runs) -> (Escape, Option<ControlToBrain>) {
        let activity = Activity {
            working: runs.working(),
            speaking: false,
            drawing: runs.drawing(),
        };
        let action = decide(activity, Panel::Open);
        let message = match action {
            Escape::Stop => runs.stop(),
            Escape::Clear => runs.clear(),
            Escape::Dismiss | Escape::Ignore => None,
        };
        (action, message)
    }

    #[test]
    fn esc_stops_then_clears_then_dismisses_with_a_cancelled_turn_pending() {
        let mut runs = answering(4);
        data(&mut runs, step(4, 0));
        state(&mut runs, 4, Phase::Done);
        data(&mut runs, end(4));
        submit(&mut runs, "a");
        assert_eq!(press(&mut runs), (Escape::Stop, None), "a is cancelled");
        assert_eq!(
            press(&mut runs),
            (
                Escape::Clear,
                Some(ControlToBrain::ClearDrawing { run: RunId(4) })
            ),
            "a cancelled turn isn't work, so the drawing clears"
        );
        assert_eq!(press(&mut runs), (Escape::Dismiss, None));
        assert_eq!(
            state(&mut runs, 5, Phase::Thinking),
            Applied {
                stop: Some(stop(5)),
                credit: 0
            },
            "a's run is stopped once it's named"
        );
        assert_eq!(press(&mut runs), (Escape::Dismiss, None));
    }

    #[test]
    fn an_older_run_ending_never_settles_a_newer_one() {
        let mut runs = answering(5);
        state(&mut runs, 6, Phase::Thinking);
        state(&mut runs, 5, Phase::Interrupted);
        data(&mut runs, end(5));
        assert!(runs.working());
        assert_eq!(runs.stop(), Some(stop(6)));
    }

    #[test]
    fn only_a_newer_run_sets_the_next_turn_s_thread() {
        let mut runs = Runs::default();
        runs.connected();
        let mut sent = Vec::new();
        for typed in ["one", "two"] {
            runs.submit(typed, |turn| {
                sent.push(turn);
                true
            })
            .expect("sent");
        }
        runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
        runs.apply_state(RunId(2), ThreadId(22), Phase::Thinking);
        runs.apply_state(RunId(1), ThreadId(11), Phase::Done);
        runs.submit("third", |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
        assert_eq!(
            sent,
            [
                DataToBrain::TextSubmit {
                    thread: None,
                    text: "one".to_owned(),
                },
                DataToBrain::TextSubmit {
                    thread: None,
                    text: "two".to_owned(),
                },
                DataToBrain::TextSubmit {
                    thread: Some(ThreadId(22)),
                    text: "third".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_fenced_older_run_s_late_failure_keeps_the_thread() {
        let mut runs = Runs::default();
        runs.connected();
        submit(&mut runs, "one");
        submit(&mut runs, "two");
        runs.apply_state(RunId(1), ThreadId(11), Phase::Thinking);
        runs.stop();
        runs.apply_state(RunId(2), ThreadId(22), Phase::Thinking);
        runs.apply_state(RunId(1), ThreadId(11), Phase::Failed);
        let mut sent = None;
        runs.submit("third", |turn| {
            sent = Some(turn);
            true
        })
        .expect("sent");
        assert_eq!(
            sent,
            Some(DataToBrain::TextSubmit {
                thread: Some(ThreadId(22)),
                text: "third".to_owned(),
            })
        );
    }

    #[test]
    fn submits_stay_under_the_brain_s_turn_cap() {
        let mut runs = Runs::default();
        runs.connected();
        let sends = std::cell::Cell::new(0);
        let send = |_| {
            sends.set(sends.get() + 1);
            true
        };
        for _ in 0..MAX_PENDING_TURNS - 1 {
            runs.submit("again", send).expect("under the cap");
        }
        assert_eq!(runs.submit("again", send), Err(Refused::Busy));
        assert_eq!(sends.get(), 63, "a refused turn never reaches the sender");
        runs.apply_state(RunId(1), ThreadId(1), Phase::Thinking);
        assert_eq!(runs.submit("again", send), Ok(()));
        assert_eq!(sends.get(), 64);
    }

    #[test]
    fn blank_long_offline_and_unsent_turns_are_refused_and_not_counted() {
        let mut runs = Runs::default();
        assert_eq!(runs.submit("hi", |_| true), Err(Refused::Offline));
        runs.connected();
        assert_eq!(runs.submit(" \n\t ", |_| true), Err(Refused::Blank));
        let long = "x".repeat(Pipe::Data.max_payload());
        assert_eq!(runs.submit(&long, |_| true), Err(Refused::TooLong));
        assert_eq!(runs.submit("hi", |_| false), Err(Refused::Offline));
        assert_eq!(runs.pending, 0);
        assert!(!runs.working());
    }

    #[test]
    fn a_lost_brain_fences_what_it_knew_and_forgets_unanswered_turns() {
        let mut runs = answering(5);
        submit(&mut runs, "pending");
        data(&mut runs, text(6, "held"));
        runs.disconnected();
        assert!(!runs.working());
        assert_eq!(runs.pending, 0);
        assert_eq!(runs.submit("hi", |_| true), Err(Refused::Offline));
        data(&mut runs, text(5, "from before"));
        assert!(runs.shown().is_none());
        runs.connected();
        assert_eq!(
            state(&mut runs, 6, Phase::Answering).credit,
            0,
            "held data went with it"
        );
        assert!(runs.shown().is_none());
        assert!(runs.working());
    }

    #[test]
    fn the_view_keeps_the_end_of_a_long_answer() {
        let mut runs = answering(1);
        data(&mut runs, text(1, &"a".repeat(TEXT_KEPT)));
        data(&mut runs, text(1, "é!"));
        let expected = format!("{}é!", "a".repeat(TEXT_KEPT - 3));
        assert_eq!(shown_text(&runs), Some((RunId(1), expected.as_str())));
    }

    #[test]
    fn the_view_cuts_long_answers_at_a_character_boundary() {
        let mut runs = answering(1);
        data(&mut runs, text(1, &"é".repeat(TEXT_KEPT / 2)));
        data(&mut runs, text(1, "!"));
        let expected = format!("{}!", "é".repeat(TEXT_KEPT / 2 - 1));
        assert_eq!(shown_text(&runs), Some((RunId(1), expected.as_str())));
    }
}
