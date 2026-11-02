//! The runs as the shell shows them: which one is moving, which ones Esc has fenced off, and the
//! placeholder view of the newest one. Plain state, no GPUI, so every rule is unit-tested.
//!
//! The fence is checked when a message is applied, not when it's received (architecture v1
//! §3.3), so messages already queued in the shell when Esc fences their run are dropped too.

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

#[derive(Debug, Default)]
pub(crate) struct Runs {
    connected: bool,
    /// The run the brain last said is thinking or answering, until it ends.
    current: Option<RunId>,
    /// The highest run Esc stopped; anything of it or older is dropped when applied.
    fence: RunId,
    /// The highest run whose drawing Esc cleared; its steps are dropped when applied.
    cleared: RunId,
    /// The highest run any `RunState` named: each new one answers one turn.
    answered: RunId,
    /// Turns sent with no `RunState` for a new run yet.
    unanswered: usize,
    /// The thread the next turn continues.
    thread: Option<ThreadId>,
    shown: Option<Shown>,
}

impl Runs {
    /// A run is moving that Esc hasn't stopped.
    pub(crate) fn working(&self) -> bool {
        self.current.is_some_and(|run| run > self.fence)
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

    /// The first Esc: fences the moving run off, keeping what's on screen (a placeholder step
    /// has no entrance, so every one shown has finished entering), and returns the `Stop` to
    /// send after.
    pub(crate) fn stop(&mut self) -> Option<ControlToBrain> {
        let run = self.current.filter(|run| *run > self.fence)?;
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
        if self.unanswered + 1 >= MAX_PENDING_TURNS {
            return Err(Refused::Busy);
        }
        let turn = DataToBrain::TextSubmit {
            thread: self.thread,
            text: text.to_owned(),
        };
        if !send(turn) {
            return Err(Refused::Offline);
        }
        self.unanswered += 1;
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
        self.current = None;
        self.unanswered = 0;
    }

    /// A `RunState`. A run newer than any before answers a turn. Only runs already named are
    /// fenced, so a fenced run never answers one.
    pub(crate) fn apply_state(&mut self, run: RunId, thread: ThreadId, phase: Phase) {
        if run > self.answered {
            self.answered = run;
            self.unanswered = self.unanswered.saturating_sub(1);
        }
        self.thread = Some(thread);
        if run <= self.fence {
            return;
        }
        match phase {
            Phase::Thinking | Phase::Answering => {
                self.current = Some(self.current.map_or(run, |current| current.max(run)));
            }
            Phase::Done | Phase::Interrupted | Phase::Failed => {
                if self.current == Some(run) {
                    self.current = None;
                }
            }
        }
    }

    /// A data frame: a newer run takes over the view, and anything fenced, cleared or older than
    /// the view is dropped.
    pub(crate) fn apply_data(&mut self, message: DataToShell) {
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
            DataToShell::RunEnd { .. } => {}
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

    use super::{Refused, Runs, TEXT_KEPT};

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

    /// Connected, with `run` answering in thread 1.
    fn answering(run: u64) -> Runs {
        let mut runs = Runs::default();
        runs.connected();
        runs.apply_state(RunId(run), ThreadId(1), Phase::Answering);
        runs
    }

    fn indices(runs: &Runs) -> Vec<u32> {
        runs.shown()
            .map(|shown| shown.steps.iter().map(|(index, _)| *index).collect())
            .unwrap_or_default()
    }

    #[test]
    fn stop_fences_the_run_and_reveals_what_is_on_screen() {
        let mut runs = answering(5);
        runs.apply_data(step(5, 0));
        runs.apply_data(step(5, 1));
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
        runs.apply_data(text(5, "kept "));
        runs.stop();
        // Already queued in the shell when Esc came.
        runs.apply_data(text(5, "late"));
        runs.apply_data(step(5, 0));
        runs.apply_state(RunId(5), ThreadId(1), Phase::Answering);
        assert!(!runs.working());
        let shown = runs.shown().expect("shown");
        assert_eq!(shown.text, "kept ");
        assert!(shown.steps.is_empty());
    }

    #[test]
    fn clear_drops_the_drawing_and_keeps_late_steps_out() {
        let mut runs = answering(5);
        runs.apply_data(text(5, "words"));
        runs.apply_data(step(5, 0));
        runs.apply_state(RunId(5), ThreadId(1), Phase::Done);
        assert!(!runs.working());
        assert_eq!(
            runs.clear(),
            Some(ControlToBrain::ClearDrawing { run: RunId(5) })
        );
        assert!(!runs.drawing());
        assert_eq!(runs.clear(), None, "nothing left to clear");
        runs.apply_data(step(5, 1));
        assert_eq!(indices(&runs), Vec::<u32>::new());
        assert_eq!(runs.shown().expect("shown").text, "words", "the text stays");
        runs.apply_data(step(6, 0));
        assert_eq!(indices(&runs), vec![0], "a newer run draws again");
    }

    #[test]
    fn a_newer_run_takes_over_the_view_and_older_data_is_dropped() {
        let mut runs = answering(5);
        runs.apply_data(text(5, "old"));
        runs.apply_data(text(6, "new"));
        runs.apply_data(text(5, " late"));
        let shown = runs.shown().expect("shown");
        assert_eq!((shown.run, shown.text.as_str()), (RunId(6), "new"));
    }

    #[test]
    fn data_before_its_run_state_is_shown() {
        let mut runs = Runs::default();
        runs.connected();
        runs.apply_data(text(3, "early"));
        assert_eq!(runs.shown().expect("shown").text, "early");
        runs.apply_state(RunId(3), ThreadId(1), Phase::Answering);
        assert!(runs.working());
    }

    #[test]
    fn run_states_answer_turns_and_set_the_thread() {
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
        assert_eq!(
            sent[0],
            DataToBrain::TextSubmit {
                thread: None,
                text: "one".to_owned(),
            }
        );
        assert_eq!(runs.unanswered, 2);
        runs.apply_state(RunId(4), ThreadId(9), Phase::Thinking);
        runs.apply_state(RunId(4), ThreadId(9), Phase::Answering);
        assert_eq!(runs.unanswered, 1, "one new run answers one turn");
        runs.stop();
        runs.apply_state(RunId(5), ThreadId(9), Phase::Thinking);
        assert_eq!(runs.unanswered, 0);
        runs.submit("three", |turn| {
            sent.push(turn);
            true
        })
        .expect("sent");
        assert!(matches!(
            sent[2],
            DataToBrain::TextSubmit {
                thread: Some(ThreadId(9)),
                ..
            }
        ));
    }

    #[test]
    fn submits_stay_under_the_brain_s_turn_cap() {
        let mut runs = Runs::default();
        runs.connected();
        for _ in 0..MAX_PENDING_TURNS - 1 {
            runs.submit("again", |_| true).expect("under the cap");
        }
        assert_eq!(runs.submit("again", |_| true), Err(Refused::Busy));
        runs.apply_state(RunId(1), ThreadId(1), Phase::Thinking);
        assert_eq!(runs.submit("again", |_| true), Ok(()));
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
        assert_eq!(runs.unanswered, 0);
    }

    #[test]
    fn a_lost_brain_fences_what_it_knew_and_forgets_unanswered_turns() {
        let mut runs = answering(5);
        runs.submit("pending", |_| true).expect("sent");
        runs.disconnected();
        assert!(!runs.working());
        assert_eq!(runs.unanswered, 0);
        assert_eq!(runs.submit("hi", |_| true), Err(Refused::Offline));
        runs.apply_data(text(5, "from before"));
        assert!(runs.shown().is_none());
        runs.connected();
        runs.apply_state(RunId(6), ThreadId(1), Phase::Answering);
        assert!(runs.working());
    }

    #[test]
    fn the_view_keeps_the_end_of_a_long_answer() {
        let mut runs = answering(1);
        runs.apply_data(text(1, &"a".repeat(TEXT_KEPT)));
        runs.apply_data(text(1, "é!"));
        let shown = runs.shown().expect("shown");
        assert!(shown.text.len() <= TEXT_KEPT);
        assert!(shown.text.ends_with("é!"));
    }
}
