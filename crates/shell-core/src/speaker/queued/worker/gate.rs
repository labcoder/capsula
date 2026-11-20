use std::time::Duration;

use kumochi_protocol::message::SegmentId;

use super::Worker;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(super) enum DropOutcome {
    Kept,
    Discarded,
}

impl Worker {
    pub(super) fn drop_unsubmitted(&mut self, id: SegmentId, now: Duration) -> DropOutcome {
        // shortcut: a staged line is committed even while paused, upgrade if desk testing hears a fenced step on resume.
        if self.engine.queue.was_staged(id) {
            self.engine.release(id, now);
            DropOutcome::Kept
        } else {
            self.engine.discard(id, now);
            DropOutcome::Discarded
        }
    }
}
