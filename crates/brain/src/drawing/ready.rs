//! A complete draw call's result waits until the shell has prepared or refused its last step, so
//! a refusal made while preparing reaches the model in the same run when that step is within
//! eight of the scene's activated bound, the shell answers within a tick, and no end path ended
//! the call first. A step refused when it activates (residency or validation), or a screen scene
//! skipped after its steps were prepared, still isn't told to the model.

use std::mem;

use kumochi_protocol::message::{MAX_READY_STEPS, RunId, SceneId};

use super::{CallError, Drawing, Output};

impl Drawing {
    /// A call that ended properly: its result goes now, or once the shell answers for the last
    /// step it committed.
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

    /// The step a finished call waits for: the last it committed, unless the shell already
    /// answered for it, refused one of its steps, or it's past the shell's ready window.
    fn awaited(&self, at: usize) -> Option<u32> {
        let call = &self.calls[at];
        if call.refusal.is_some() {
            return None;
        }
        let last = call.last?;
        let state = &self.scenes[self.live(call.drew_in()?)?].state;
        // shortcut: a step past the window prepares only once speech activates an earlier one,
        // so its refusal stays unheard, and a cut's dropped steps count as ahead; give such calls
        // a speech-paced wait if late refusals show up there.
        let ahead =
            usize::try_from(last.saturating_add(1).saturating_sub(state.activated())).ok()?;
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

    /// The shell refused `scene`'s step `index`: an open or held call that committed it or a
    /// later step ends with `error` when it finishes or is released, even before the queued cut
    /// reaches it.
    pub(super) fn refused(&mut self, scene: SceneId, index: u32, error: &CallError) {
        for at in self.open_in(scene) {
            let call = &mut self.calls[at];
            if call.last.is_some_and(|last| last >= index) {
                call.refusal.get_or_insert_with(|| error.clone());
            }
        }
    }

    /// A ready-wait tick passed: a held call that heard nothing since the last tick gets its result.
    pub(super) fn ready_wait(&mut self, out: &mut Output) {
        for at in 0..self.calls.len() {
            let call = &mut self.calls[at];
            if call.done || call.awaits.is_none() || mem::take(&mut call.heard) {
                continue;
            }
            self.release(at, out);
        }
    }

    /// A held call whose scene left gets its result, a refusal already received included.
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

    /// Runs up to `closed` ended, so no later refusal can be heard: their held calls get their
    /// results instead of being interrupted, a refusal already received included.
    pub(super) fn release_closed(&mut self, closed: RunId, out: &mut Output) {
        for at in 0..self.calls.len() {
            let call = &self.calls[at];
            if call.run <= closed && !call.done && call.awaits.is_some() {
                self.release(at, out);
            }
        }
    }

    /// A finished call waits for the shell, so the coordinator keeps the ready-wait timer ticking.
    pub fn holding(&self) -> bool {
        self.calls
            .iter()
            .any(|call| !call.done && call.awaits.is_some())
    }

    fn release(&mut self, at: usize, out: &mut Output) {
        if let Some(error) = self.calls[at].refusal.take() {
            return self.end(at, Some(error), out);
        }
        let after_records = self.calls[at].after_records;
        self.end(at, None, out);
        out.calls
            .last_mut()
            .expect("end pushed this call's result")
            .after_records = after_records;
    }
}
