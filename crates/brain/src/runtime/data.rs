//! The data pipe's outgoing queue: frames in order, sent only against the byte credit the shell
//! grants, so a shell that stops reading can't make the brain buffer without bound.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long the shell may leave the queue blocked on credit before the run ends as
/// `view_stalled` (architecture v1 §3.2) [guess].
pub(crate) const STALL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct DataQueue {
    frames: VecDeque<Vec<u8>>,
    credit: usize,
    window: usize,
    /// When the front frame first had to wait for credit.
    blocked_since: Option<Instant>,
}

impl DataQueue {
    /// A queue that may send `window` bytes before the first grant.
    pub(crate) fn new(window: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            credit: window,
            window,
            blocked_since: None,
        }
    }

    pub(crate) fn push(&mut self, frame: Vec<u8>) {
        self.frames.push_back(frame);
    }

    /// Adds credit, never past the window, so a confused shell can't unbound the queue.
    pub(crate) fn grant(&mut self, bytes: u32) {
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        self.credit = self.credit.saturating_add(bytes).min(self.window);
    }

    /// The next frame, if the credit covers it.
    pub(crate) fn pop(&mut self, now: Instant) -> Option<Vec<u8>> {
        let len = self.frames.front()?.len();
        if len > self.credit {
            self.blocked_since.get_or_insert(now);
            return None;
        }
        self.credit -= len;
        self.blocked_since = None;
        self.frames.pop_front()
    }

    /// Whether frames have waited on credit for longer than [`STALL`].
    pub(crate) fn stalled(&self, now: Instant) -> bool {
        self.blocked_since
            .is_some_and(|since| now.duration_since(since) >= STALL)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{DataQueue, STALL};

    #[test]
    fn frames_go_in_order_and_only_against_credit() {
        let start = Instant::now();
        let mut queue = DataQueue::new(10);
        queue.push(vec![1; 6]);
        queue.push(vec![2; 6]);
        assert_eq!(queue.pop(start), Some(vec![1; 6]));
        assert_eq!(queue.pop(start), None, "4 bytes of credit left");
        queue.grant(1);
        assert_eq!(queue.pop(start), None);
        queue.grant(1);
        assert_eq!(queue.pop(start), Some(vec![2; 6]));
        assert_eq!(queue.pop(start), None, "empty");
    }

    #[test]
    fn credit_never_exceeds_the_window() {
        let mut queue = DataQueue::new(8);
        queue.grant(u32::MAX);
        queue.push(vec![0; 9]);
        assert_eq!(queue.pop(Instant::now()), None);
    }

    #[test]
    fn a_queue_blocked_on_credit_stalls_after_the_limit() {
        let start = Instant::now();
        let mut queue = DataQueue::new(5);
        assert!(!queue.stalled(start + STALL * 2), "empty never stalls");
        queue.push(vec![0; 3]);
        queue.push(vec![0; 5]);
        assert!(queue.pop(start).is_some());
        assert_eq!(queue.pop(start), None, "2 bytes of credit left");
        assert!(!queue.stalled(start + Duration::from_millis(1999)));
        assert!(queue.stalled(start + STALL));
        queue.grant(3);
        assert!(queue.pop(start + STALL).is_some());
        assert!(!queue.stalled(start + STALL * 3), "progress resets it");
    }
}
