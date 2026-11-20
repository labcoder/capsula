use std::time::Duration;

use kumochi_protocol::message::SegmentId;

use super::Worker;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub(super) enum DropOutcome {
    AwaitingCopy,
    PredecessorPending,
    Kept,
    Discarded,
}

impl Worker {
    pub(super) fn drop_unsubmitted(&mut self, id: SegmentId, now: Duration) -> DropOutcome {
        if let Some(segment) = self.engine.queue.segment(id)
            && self.engine.offered > segment.first
            && !self.engine.queue.discarded(id)
            && let Some(route) = &self.physical
        {
            let control = route.transport.control();
            let live = route.retire_deadline.is_none();
            let handed = if live {
                control.inhibit();
                control.inhibited_handoff()
            } else {
                control.fenced_handoff()
            };
            let Some(through) = handed else {
                return DropOutcome::AwaitingCopy;
            };
            let first = segment.first;
            let predecessor_pending =
                live && through != 0 && through < first && self.engine.progress.consumed < first;
            if through > first || predecessor_pending {
                if live && !self.engine.inhibited() {
                    control.resume();
                }
                if predecessor_pending {
                    return DropOutcome::PredecessorPending;
                }
                self.engine.release(id, now);
                return DropOutcome::Kept;
            }
            self.recover(now);
        }

        self.engine.discard(id, now);
        DropOutcome::Discarded
    }
}
