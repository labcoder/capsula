//! A thread's model choice, read and written by id. Kumo's main chat takes the same read and write
//! as a task. Answers go only to the pair that asked: each request is stamped with the session at
//! ingress, and an answer for an earlier session is dropped. Reads keep at most
//! [`MAX_PENDING_MODEL_READS`] slots from admission until the reader answers, cancelled ones
//! included, so a shell that reconnects or floods can't grow the reader's queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kumochi_protocol::message::{
    ChoiceRead, ControlToShell, MAX_PENDING_MODEL_READS, ModelChoice, ThreadId,
};
use tokio::time::Instant;

use super::{Coordinator, Effect, StoreOp};
use crate::recall::control::{Control, READ_TIME};

/// One read on the page reader, ordered after every write accepted before it.
#[derive(Clone, Debug)]
pub struct ModelRead {
    pub(crate) session: u64,
    pub(crate) request: u32,
    pub(crate) thread: ThreadId,
    /// Ends at the deadline set at admission, or when the pair that asked is gone.
    pub(crate) control: Control,
}

impl PartialEq for ModelRead {
    fn eq(&self, other: &Self) -> bool {
        (self.session, self.request, self.thread) == (other.session, other.request, other.thread)
    }
}

#[derive(Debug)]
struct Slot {
    session: u64,
    request: u32,
    thread: ThreadId,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    /// Handed to the reader; a parked read waits for its new thread's row to be queued.
    sent: bool,
    /// Already answered `Failed` at its deadline; the reader's late answer is dropped.
    expired: bool,
}

/// The pair's session and its admitted reads.
#[derive(Debug, Default)]
pub(super) struct ModelChoices {
    session: u64,
    slots: Vec<Slot>,
    /// The deadline the runtime's timer is set to, cleared when it fires.
    armed: Option<Instant>,
}

impl Coordinator {
    #[cfg(test)]
    pub(crate) fn model_session(&self) -> u64 {
        self.model_choices.session
    }

    /// A pair starts or is lost: its reads are cancelled and none of its answers go out again.
    pub(super) fn begin_model_session(&mut self) {
        let choices = &mut self.model_choices;
        choices.session = choices
            .session
            .checked_add(1)
            .expect("pair sessions never wrap");
        choices.slots.retain(|slot| {
            slot.cancel.store(true, Ordering::SeqCst);
            slot.sent
        });
    }

    pub(super) fn set_model(
        &self,
        request: u32,
        thread: ThreadId,
        choice: Option<ModelChoice>,
        effects: &mut Vec<Effect>,
    ) {
        // No build serves a local server yet, so none is saved as a thread's route, and while a
        // local default is saved no thread's route changes: one that inherits it stays local.
        if self.local_default
            || choice.as_ref().is_some_and(|choice| {
                choice.connection == kumochi_protocol::message::ConnectionKind::LocalChat
            })
        {
            effects.push(Effect::Control(ControlToShell::ThreadModelSet {
                request,
                thread,
                result: ChoiceRead::Failed,
            }));
            return;
        }
        effects.push(Effect::Store(StoreOp::SetModel {
            session: self.model_choices.session,
            request,
            thread,
            choice,
        }));
    }

    pub(super) fn model_set(
        &self,
        session: u64,
        request: u32,
        thread: ThreadId,
        result: ChoiceRead,
        effects: &mut Vec<Effect>,
    ) {
        if session == self.model_choices.session {
            effects.push(Effect::Control(ControlToShell::ThreadModelSet {
                request,
                thread,
                result,
            }));
        }
    }

    /// Admits a read, or refuses it at once when every slot is held. A repeat of a request still
    /// held is ignored, so that request is answered once.
    pub(super) fn read_model(&mut self, request: u32, thread: ThreadId, effects: &mut Vec<Effect>) {
        let session = self.model_choices.session;
        let slots = &mut self.model_choices.slots;
        if slots
            .iter()
            .any(|slot| slot.session == session && slot.request == request)
        {
            // The first read with this number answers it, once.
            return;
        }
        if slots.len() >= MAX_PENDING_MODEL_READS {
            effects.push(Effect::Control(ControlToShell::ThreadModelRead {
                request,
                thread,
                result: ChoiceRead::Failed,
            }));
            return;
        }
        slots.push(Slot {
            session,
            request,
            thread,
            deadline: Instant::now() + READ_TIME,
            cancel: Arc::new(AtomicBool::new(false)),
            sent: false,
            expired: false,
        });
    }

    /// Fails reads past their deadline, then hands the reader every parked read whose thread has
    /// no new row waiting to be queued, so the read is stamped after the write that creates it;
    /// arms the earliest deadline still owed an answer, parked or handed out.
    pub(super) fn release_model_reads(&mut self, effects: &mut Vec<Effect>) {
        if self.model_choices.slots.is_empty() {
            return;
        }
        self.expire_model_reads(effects);
        if self.model_choices.slots.iter().any(|slot| !slot.sent) {
            let unborn: Vec<ThreadId> = self
                .ending
                .iter()
                .chain(self.current.as_ref())
                .filter_map(|run| match &run.begin {
                    Some(StoreOp::BeginTurn {
                        thread,
                        title: Some(_),
                        ..
                    }) => Some(*thread),
                    _ => None,
                })
                .collect();
            for slot in self
                .model_choices
                .slots
                .iter_mut()
                .filter(|slot| !slot.sent && !unborn.contains(&slot.thread))
            {
                slot.sent = true;
                effects.push(Effect::Store(StoreOp::ReadModel(ModelRead {
                    session: slot.session,
                    request: slot.request,
                    thread: slot.thread,
                    control: Control::new(slot.deadline.into_std())
                        .observe(Arc::clone(&slot.cancel)),
                })));
            }
        }
        let choices = &mut self.model_choices;
        let next = choices
            .slots
            .iter()
            .filter(|slot| slot.session == choices.session && !slot.expired)
            .map(|slot| slot.deadline)
            .min();
        if let Some(at) = next
            && choices.armed.is_none_or(|armed| at < armed)
        {
            choices.armed = Some(at);
            effects.push(Effect::ArmModelReads { at });
        }
    }

    /// The timer fired; the expiry itself runs with every input's release.
    pub(super) fn model_reads_due(&mut self) {
        self.model_choices.armed = None;
    }

    /// Answers `Failed` once for each read past its deadline, before any parked read is handed
    /// out, so a store acknowledgement taken ahead of the timer can't send an expired read. A
    /// parked one frees its slot; one the reader holds is cancelled and keeps its slot until the
    /// reader lets it go.
    fn expire_model_reads(&mut self, effects: &mut Vec<Effect>) {
        let now = Instant::now();
        let choices = &mut self.model_choices;
        choices.slots.retain_mut(|slot| {
            if slot.expired || slot.deadline > now {
                return true;
            }
            slot.expired = true;
            slot.cancel.store(true, Ordering::SeqCst);
            if slot.session == choices.session {
                effects.push(Effect::Control(ControlToShell::ThreadModelRead {
                    request: slot.request,
                    thread: slot.thread,
                    result: ChoiceRead::Failed,
                }));
            }
            slot.sent
        });
    }

    /// The reader answered: the slot is free again, and the answer goes out only to its pair.
    pub(super) fn model_read(
        &mut self,
        read: &ModelRead,
        result: ChoiceRead,
        effects: &mut Vec<Effect>,
    ) {
        let choices = &mut self.model_choices;
        let Some(at) = choices.slots.iter().position(|slot| {
            slot.sent && slot.session == read.session && slot.request == read.request
        }) else {
            return;
        };
        let slot = choices.slots.swap_remove(at);
        if slot.expired || slot.session != choices.session {
            return;
        }
        // Finished in time but taken after the deadline: still the one Failed it was promised.
        let result = if slot.deadline > Instant::now() {
            result
        } else {
            ChoiceRead::Failed
        };
        effects.push(Effect::Control(ControlToShell::ThreadModelRead {
            request: read.request,
            thread: read.thread,
            result,
        }));
    }
}
