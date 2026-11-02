//! Deleting threads in the core loop. A delete ends the thread's current run and goes to the
//! store; until it's answered, and after a hide or removal until every submit the shell sent
//! before it has been read, a turn naming the thread is refused before it gets a run. The answer
//! retires the thread's runs and scenes, so nothing of them reaches the store or the shell again.

use kumochi_protocol::message::{
    ControlToShell, DataToShell, Deleted, MAX_PENDING_DELETES, RunId, SceneId, ThreadId,
};

use super::{Coordinator, Effect, Input, Notify, Stale, StoreOp};
use crate::store::{Interruption, RunEnd};

/// Deletes in flight before another is refused: those that outlived earlier connections, whose
/// shells may have restarted and forgotten them, and the current shell's.
const MAX_DELETING: usize = 2 * MAX_PENDING_DELETES;

/// A delete sent to the store and not answered yet.
#[derive(Debug)]
pub(super) struct Deleting {
    thread: ThreadId,
    /// How many submits this connection's shell sent before the delete; `None` until a
    /// connection after the one that sent it names it again.
    watermark: Option<u32>,
}

impl Coordinator {
    /// A delete is waiting on the store.
    pub fn deletes_pending(&self) -> bool {
        !self.deleting.is_empty()
    }

    /// A new connection: the old one's counts and fences mean nothing to it.
    pub(super) fn begin_deletes(&mut self) {
        for entry in &mut self.deleting {
            entry.watermark = None;
        }
        self.fenced.clear();
        self.crossed.clear();
        self.turns_read = 0;
    }

    /// A typed turn, counted before it's classified: one naming a thread being deleted, or one
    /// the shell sent before deleting it, or a blank one, is refused before it gets a run.
    pub(super) fn typed(
        &mut self,
        thread: Option<ThreadId>,
        text: String,
        effects: &mut Vec<Effect>,
    ) {
        let Some(read) = self.turns_read.checked_add(1) else {
            effects.push(Effect::Broken("more turns than a connection counts"));
            return;
        };
        self.turns_read = read;
        if self.admits(thread) && !text.trim().is_empty() {
            self.turn(thread, text, effects);
        } else {
            let refused = ControlToShell::TurnRefused { thread };
            effects.push(Effect::Control(refused));
        }
    }

    /// Classifies the turn just counted. Fences the turn has passed retire after.
    fn admits(&mut self, thread: Option<ThreadId>) -> bool {
        let read = self.turns_read;
        let refused = thread.is_some_and(|thread| {
            self.deleting.iter().any(|entry| entry.thread == thread)
                || self
                    .fenced
                    .iter()
                    .any(|&(fenced, before)| fenced == thread && read <= before)
        });
        self.fenced.retain(|&(_, before)| before > read);
        !refused
    }

    /// A delete or its answer.
    pub(super) fn deletion(
        &mut self,
        input: &Input,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        match *input {
            Input::Delete {
                thread,
                after_turns,
                resend,
            } => {
                let crossed = self.crossed.iter().position(|&crossed| crossed == thread);
                if let Some(at) = crossed {
                    self.crossed.swap_remove(at);
                }
                // This connection already carries the answer a crossed resend asks for.
                if !(crossed.is_some() && resend) {
                    self.delete(thread, after_turns, effects);
                }
                Ok(())
            }
            Input::Deleted {
                thread,
                outcome,
                last_run,
            } => self.deleted(thread, outcome, last_run, effects),
            _ => Err(Stale),
        }
    }

    /// The shell asked to delete `thread`. A delete already in flight answers for this one too.
    fn delete(&mut self, thread: ThreadId, after_turns: u32, effects: &mut Vec<Effect>) {
        if let Some(entry) = self
            .deleting
            .iter_mut()
            .find(|entry| entry.thread == thread)
        {
            entry.watermark = Some(after_turns);
            return;
        }
        if self.deleting.len() >= MAX_DELETING {
            let refused = ControlToShell::ThreadDeleted {
                thread,
                outcome: Deleted::NotHidden,
                last_run: None,
            };
            effects.push(Effect::Control(refused));
            return;
        }
        if let Some(current) = self.current.take_if(|current| current.thread == thread) {
            let deleted = RunEnd::Interrupted(Interruption::Deleted);
            self.end(current, deleted, None, Notify::Shell, effects);
        }
        self.deleting.push(Deleting {
            thread,
            watermark: Some(after_turns),
        });
        effects.push(Effect::Store(StoreOp::Delete { thread }));
    }

    /// The store answered a delete. Once the thread is hidden its runs and scenes retire: their
    /// later replies are stale, and the shell hears which scenes left.
    fn deleted(
        &mut self,
        thread: ThreadId,
        outcome: Deleted,
        stored: Option<RunId>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), Stale> {
        let at = self
            .deleting
            .iter()
            .position(|entry| entry.thread == thread)
            .ok_or(Stale)?;
        let Deleting { watermark, .. } = self.deleting.swap_remove(at);
        if watermark.is_none() {
            self.crossed.push(thread);
        }
        if outcome == Deleted::NotHidden {
            let answer = ControlToShell::ThreadDeleted {
                thread,
                outcome,
                last_run: None,
            };
            effects.push(Effect::Control(answer));
            return Ok(());
        }
        // Before the runs retire: a turn still held behind an older outcome has no row yet.
        let named = self
            .current
            .iter()
            .chain(&self.ending)
            .filter(|run| run.thread == thread)
            .map(|run| run.id)
            .max();
        if let Some(before) = watermark.filter(|&before| self.turns_read < before) {
            if self.fenced.len() >= MAX_PENDING_DELETES {
                effects.push(Effect::Broken("more deletes fenced than the shell holds"));
            } else {
                self.fenced.push((thread, before));
            }
        }
        debug_assert!(
            self.current.as_ref().is_none_or(|run| run.thread != thread),
            "a turn naming a thread being deleted is refused"
        );
        self.ending.retain(|run| run.thread != thread);
        let removed: Vec<SceneId> = self
            .scenes
            .iter()
            .filter(|state| state.thread == thread)
            .map(|state| state.scene)
            .collect();
        self.scenes.retain(|state| state.thread != thread);
        effects.push(Effect::Control(ControlToShell::ThreadDeleted {
            thread,
            outcome,
            last_run: stored.max(named),
        }));
        if !removed.is_empty() {
            effects.push(Effect::RevokeScenes {
                scenes: removed.clone(),
            });
            effects.push(Effect::Data(DataToShell::ScenesRemoved { scenes: removed }));
        }
        Ok(())
    }
}
