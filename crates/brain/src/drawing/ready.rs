//! A complete draw call's result waits until the shell has prepared or refused its last step,
//! so a refusal made while preparing reaches the model in the same run. A step refused when it
//! activates (residency or validation), or a screen scene skipped after its steps were prepared,
//! still isn't told to the model.

use std::mem;
use std::time::Duration;

use kumochi_protocol::message::{MAX_READY_STEPS, RunId, SceneId};

use super::{Drawing, Output};

/// How long a held call waits with no answer from the shell for its scene: four times the
/// shell's 250 ms preparation deadline. A tick that finds no answer since the last one ends it.
pub const READY_WAIT: Duration = Duration::from_secs(1);

impl Drawing {
    /// A call that ended properly: its result goes now, or once the shell answers for its
    /// scene's last committed step.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the result tells ignored trailing text whenever it goes"
    )]
    pub(super) fn finished(&mut self, at: usize, after_records: bool, out: &mut Output) {
        self.calls[at].after_records = after_records;
        let Some(last) = self.awaited(at) else {
            return self.release(at, out);
        };
        let call = &mut self.calls[at];
        call.awaits = Some(last);
        // A tick already under way counts as heard, so the wait lasts at least one whole tick.
        call.heard = true;
    }

    /// The step a finished call waits for: its scene's newest, unless the shell already answered
    /// for it or it's past the shell's ready window.
    fn awaited(&self, at: usize) -> Option<u32> {
        let call = &self.calls[at];
        let state = &self.scenes[self.live(call.drew_in()?)?].state;
        let last = state.revision().checked_sub(1)?;
        // shortcut: a step past the window prepares only once speech activates an earlier one,
        // so its refusal stays unheard, and a cut's dropped steps count as ahead; give such calls
        // a speech-paced wait if late refusals show up there.
        let ahead = usize::try_from(state.revision().saturating_sub(state.activated())).ok()?;
        (call.answered.is_none_or(|answered| answered < last) && ahead <= MAX_READY_STEPS)
            .then_some(last)
    }

    /// The shell prepared or first painted `scene`'s step `index`: a call waiting for it, or for
    /// an earlier step, gets its result.
    pub(super) fn answered(&mut self, scene: SceneId, index: u32, out: &mut Output) {
        for at in self.open_in(scene) {
            let call = &mut self.calls[at];
            call.heard = true;
            call.answered = call.answered.max(Some(index));
            if call.awaits.is_some_and(|last| last <= index) {
                self.release(at, out);
            }
        }
    }

    /// [`READY_WAIT`] passed: a held call that heard nothing since the last tick gets its result.
    pub(super) fn ready_wait(&mut self, out: &mut Output) {
        for at in 0..self.calls.len() {
            let call = &mut self.calls[at];
            if call.done || call.awaits.is_none() || mem::take(&mut call.heard) {
                continue;
            }
            self.release(at, out);
        }
    }

    /// A held call whose scene left gets its result: nothing of it was refused.
    pub(super) fn release_left(&mut self, out: &mut Output) {
        for at in 0..self.calls.len() {
            let call = &self.calls[at];
            if !call.done
                && call.awaits.is_some()
                && call
                    .drew_in()
                    .is_none_or(|scene| self.live(scene).is_none())
            {
                self.release(at, out);
            }
        }
    }

    /// Runs up to `closed` ended, so nobody can hear a refusal now: their held calls get their
    /// results instead of being interrupted.
    pub(super) fn release_closed(&mut self, closed: RunId, out: &mut Output) {
        for at in 0..self.calls.len() {
            let call = &self.calls[at];
            if call.run <= closed && !call.done && call.awaits.is_some() {
                self.release(at, out);
            }
        }
    }

    /// A finished call waits for the shell, so the coordinator keeps [`READY_WAIT`] ticking.
    pub fn holding(&self) -> bool {
        self.calls
            .iter()
            .any(|call| !call.done && call.awaits.is_some())
    }

    fn release(&mut self, at: usize, out: &mut Output) {
        let after_records = self.calls[at].after_records;
        self.end(at, None, out);
        out.calls
            .last_mut()
            .expect("end pushed this call's result")
            .after_records = after_records;
    }
}
