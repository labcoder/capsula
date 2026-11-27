//! The chat's committed model choice, as the brain last answered it by id. The thread list is
//! never consulted: a row can survive a reconnect, and a promoted chat may have none.
//!
//! The tracker keys itself on the pair, the selection and the thread, so focus, a New chat's
//! promotion, Kumo's id arriving with settings, a confirmed delete and a reconnect each forget the
//! answer and ask again. Only the outstanding request of each kind is matched, by id, kind and
//! thread, and an answer asked under an older key retires its slot without applying. A found
//! choice replaces the state only at a revision at least the key's newest, and nothing lowers it.

use std::time::{Duration, Instant};

use kumochi_protocol::message::{
    ChoiceRead, ChoiceRev, ControlToBrain, ControlToShell, ModelChoice, SavedChoice, ThreadId,
    ThreadOp,
};

/// How long a model write may go unanswered before the shell stops waiting for it and reads
/// again [guess].
pub const MODEL_WRITE_TIME: Duration = Duration::from_secs(5);

/// How long after a failed read the shell asks again for the same chat [guess].
// shortcut: the retry goes with the first link batch after the pause, not on its own timer;
// a view that shows the choice arms one.
pub const MODEL_READ_RETRY: Duration = Duration::from_secs(1);

/// What the shell knows of the current chat's model choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// No thread yet: a New chat inherits the global default until its first turn makes one.
    NoThread,
    /// Not answered for this chat, a write is unanswered, or nothing read since a write's answer
    /// was lost has settled it.
    Unknown,
    /// The brain's committed answer.
    Found {
        /// The saved choice.
        choice: SavedChoice,
        /// The revision that wrote it.
        revision: ChoiceRev,
    },
    /// No live thread has the chat's id: it was deleted.
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    session: u64,
    selection: u64,
    thread: ThreadId,
}

#[derive(Debug, Default)]
pub(super) struct Tracker {
    /// The connection's generation; none while disconnected.
    session: Option<u64>,
    /// The last request number issued on this connection; none are issued past `u32::MAX`.
    issued: u32,
    key: Option<Key>,
    /// The read in flight, and the key it was asked under.
    read: Option<(u32, Key)>,
    /// The write in flight, the key it was asked under, and when the shell stops waiting.
    write: Option<(u32, Key, Instant)>,
    /// A read is owed for the current key.
    wanted: bool,
    /// A read failed: the next one waits until then.
    retry_at: Option<Instant>,
    /// A write's answer was lost or failed: only a read issued after this request settles.
    settle_after: Option<u32>,
    known: Option<Choice>,
    newest: Option<ChoiceRev>,
}

impl Tracker {
    pub(super) fn connected(&mut self, session: u64) {
        *self = Self {
            session: Some(session),
            ..Self::default()
        };
    }

    pub(super) fn disconnected(&mut self) {
        *self = Self::default();
    }

    /// Follows the selection; a changed key forgets the answer and owes a read.
    pub(super) fn sync(&mut self, selection: u64, thread: Option<ThreadId>) {
        let key = self.session.zip(thread).map(|(session, thread)| Key {
            session,
            selection,
            thread,
        });
        if key == self.key {
            return;
        }
        self.key = key;
        self.write = None;
        self.settle_after = None;
        self.retry_at = None;
        self.known = None;
        self.newest = None;
        self.wanted = key.is_some();
    }

    pub(super) fn choice(&self, selection: u64, thread: Option<ThreadId>) -> Choice {
        let Some(thread) = thread else {
            return Choice::NoThread;
        };
        let current = self.key.is_some_and(|key| {
            (key.selection, key.thread) == (selection, thread) && self.session == Some(key.session)
        });
        if !current || self.write.is_some() || self.settle_after.is_some() {
            return Choice::Unknown;
        }
        self.known.clone().unwrap_or(Choice::Unknown)
    }

    fn issue(&mut self) -> Option<u32> {
        self.issued = self.issued.checked_add(1)?;
        Some(self.issued)
    }

    /// The owed read, once the one in flight has answered.
    pub(super) fn read(&mut self, now: Instant) -> Option<ControlToBrain> {
        self.expire(now);
        let key = self.key?;
        if !self.wanted || self.read.is_some() || self.retry_at.is_some_and(|at| at > now) {
            return None;
        }
        let request = self.issue()?;
        self.read = Some((request, key));
        self.wanted = false;
        self.retry_at = None;
        Some(ControlToBrain::ReadModel {
            request,
            thread: key.thread,
        })
    }

    /// A Set, or a Clear with `None`, only over a current answer that names a live thread.
    pub(super) fn write(
        &mut self,
        choice: Option<ModelChoice>,
        now: Instant,
    ) -> Option<ControlToBrain> {
        self.expire(now);
        let key = self.key?;
        if self.write.is_some()
            || self.settle_after.is_some()
            || !matches!(self.known, Some(Choice::Found { .. }))
        {
            return None;
        }
        let request = self.issue()?;
        self.write = Some((request, key, now + MODEL_WRITE_TIME));
        Some(ControlToBrain::ThreadOp(ThreadOp::SetModel {
            request,
            thread: key.thread,
            choice,
        }))
    }

    /// The message wasn't sent: a read is owed again, and a write leaves the choice unsettled.
    pub(super) fn unsent(&mut self, message: &ControlToBrain) {
        match message {
            ControlToBrain::ReadModel { request, .. }
                if self.read.is_some_and(|(id, _)| id == *request) =>
            {
                self.read = None;
                self.wanted = true;
            }
            ControlToBrain::ThreadOp(ThreadOp::SetModel { request, .. }) => {
                self.lose_write(*request);
            }
            _ => {}
        }
    }

    fn lose_write(&mut self, request: u32) {
        if self.write.is_some_and(|(id, ..)| id == request) {
            self.write = None;
            self.settle_after = Some(request);
            self.wanted = true;
        }
    }

    fn expire(&mut self, now: Instant) {
        if let Some((request, _, at)) = self.write
            && at <= now
        {
            self.lose_write(request);
        }
    }

    /// A write past its deadline is lost before its answer is matched, so a late answer can't
    /// stand in for the read that settles it.
    pub(super) fn apply(&mut self, reply: &ControlToShell, now: Instant) {
        self.expire(now);
        let (request, thread, result, written) = match reply {
            ControlToShell::ThreadModelSet {
                request,
                thread,
                result,
            } => (*request, *thread, result, true),
            ControlToShell::ThreadModelRead {
                request,
                thread,
                result,
            } => (*request, *thread, result, false),
            _ => return,
        };
        let asked = if written {
            self.write
                .filter(|(id, key, _)| *id == request && key.thread == thread)
                .map(|(_, key, _)| key)
        } else {
            self.read
                .filter(|(id, _)| *id == request)
                .map(|(_, key)| key)
        };
        let Some(asked) = asked else {
            return;
        };
        if written {
            self.write = None;
        } else {
            self.read = None;
        }
        if Some(asked) != self.key {
            return;
        }
        if asked.thread != thread {
            self.wanted = true;
            return;
        }
        let settles = written || self.settle_after.is_none_or(|after| request > after);
        match result {
            ChoiceRead::Failed if written => {
                self.settle_after = Some(request);
                self.wanted = true;
            }
            ChoiceRead::Failed => {
                self.wanted = true;
                self.retry_at = Some(now + MODEL_READ_RETRY);
            }
            _ if self.known == Some(Choice::Missing) => {}
            ChoiceRead::Missing => {
                self.known = Some(Choice::Missing);
                if settles {
                    self.settle_after = None;
                }
            }
            ChoiceRead::Found { choice, revision } => {
                if self.newest.is_some_and(|newest| *revision < newest) {
                    return;
                }
                self.newest = Some(*revision);
                self.known = Some(Choice::Found {
                    choice: choice.clone(),
                    revision: *revision,
                });
                if settles {
                    self.settle_after = None;
                }
            }
        }
    }
}
