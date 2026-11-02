//! The data pipe's outgoing queue: frames in order, sent only against the byte credit the shell
//! grants, so a shell that stops reading can't make the brain send without bound.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use capsula_protocol::message::{MAX_PENDING_PAGES, RunId};

/// How long the shell may leave the queue blocked on credit before the run ends as
/// `view_stalled` (architecture v1 §3.2) [guess].
pub(crate) const STALL: Duration = Duration::from_secs(2);
/// What a queued frame costs beyond its bytes.
const ENTRY: usize = std::mem::size_of::<(Option<RunId>, Vec<u8>)>();

#[derive(Debug)]
pub(crate) struct DataQueue {
    frames: VecDeque<(Option<RunId>, Vec<u8>)>,
    pending_pages: usize,
    writing_page: bool,
    /// The memory the queued frames hold.
    bytes: usize,
    credit: usize,
    window: usize,
    /// Since when bytes have waited, on credit or on a write the shell isn't draining.
    waiting_since: Option<Instant>,
}

impl DataQueue {
    /// A queue that may send `window` bytes before the first grant.
    pub(crate) fn new(window: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            pending_pages: 0,
            writing_page: false,
            bytes: 0,
            credit: window,
            window,
            waiting_since: None,
        }
    }

    pub(crate) fn push(&mut self, run: Option<RunId>, frame: Vec<u8>) {
        self.bytes += frame.len() + ENTRY;
        self.frames.push_back((run, frame));
    }

    pub(crate) fn admit_page(&mut self) -> bool {
        if self.pending_pages == MAX_PENDING_PAGES {
            return false;
        }
        self.pending_pages += 1;
        true
    }

    /// The memory the queued frames hold, for pausing what feeds the queue.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(super) fn frames_for_tests(&self) -> usize {
        self.frames.len()
    }

    /// Drops a stopped run's frames that haven't started writing; the shell would drop them.
    pub(crate) fn revoke(&mut self, run: RunId) {
        self.frames.retain(|(owner, _)| *owner != Some(run));
        self.bytes = self
            .frames
            .iter()
            .map(|(_, frame)| frame.len() + ENTRY)
            .sum();
    }

    /// Adds credit, never past the window, so a confused shell can't unbound the queue.
    pub(crate) fn grant(&mut self, bytes: u32) {
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        self.credit = self.credit.saturating_add(bytes).min(self.window);
    }

    /// The next frame, if the credit covers it. Call [`DataQueue::wrote`] once it's written.
    pub(crate) fn pop(&mut self, now: Instant) -> Option<Vec<u8>> {
        let len = self.frames.front()?.1.len();
        self.waiting_since.get_or_insert(now);
        if len > self.credit {
            return None;
        }
        self.credit -= len;
        self.bytes -= len + ENTRY;
        self.writing_page = self
            .frames
            .front()
            .is_some_and(|(owner, _)| owner.is_none());
        self.frames.pop_front().map(|(_, frame)| frame)
    }

    /// The frame from the last [`DataQueue::pop`] reached the pipe: progress.
    pub(crate) fn wrote(&mut self) {
        if self.writing_page {
            self.pending_pages -= 1;
            self.writing_page = false;
        }
        self.waiting_since = None;
    }

    /// Whether bytes have waited without progress for [`STALL`] or longer.
    pub(crate) fn stalled(&self, now: Instant) -> bool {
        self.waiting_since
            .is_some_and(|since| now.duration_since(since) >= STALL)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use capsula_protocol::message::RunId;

    use super::{DataQueue, ENTRY, STALL};

    const A: RunId = RunId(1);
    const B: RunId = RunId(2);

    #[test]
    fn frames_go_in_order_and_only_against_credit() {
        let start = Instant::now();
        let mut queue = DataQueue::new(10);
        queue.push(Some(A), vec![1; 6]);
        queue.push(Some(A), vec![2; 6]);
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
        queue.push(Some(A), vec![0; 9]);
        assert_eq!(queue.pop(Instant::now()), None);
    }

    #[test]
    fn a_stopped_run_loses_only_its_unsent_frames() {
        let start = Instant::now();
        let mut queue = DataQueue::new(100);
        queue.push(Some(A), vec![1]);
        queue.push(Some(B), vec![2]);
        queue.push(Some(A), vec![3]);
        queue.revoke(A);
        assert_eq!(queue.bytes(), 1 + ENTRY);
        assert_eq!(queue.pop(start), Some(vec![2]));
        assert_eq!(queue.pop(start), None);
        assert_eq!(queue.bytes(), 0);
    }

    #[test]
    fn waiting_on_credit_stalls_after_the_limit() {
        let start = Instant::now();
        let mut queue = DataQueue::new(5);
        assert!(!queue.stalled(start + STALL * 2), "empty never stalls");
        queue.push(Some(A), vec![0; 3]);
        queue.push(Some(A), vec![0; 5]);
        assert_eq!(queue.pop(start), Some(vec![0; 3]));
        queue.wrote();
        assert_eq!(queue.pop(start), None, "2 bytes of credit left");
        assert!(!queue.stalled(start + Duration::from_millis(1999)));
        assert!(queue.stalled(start + STALL));
        queue.grant(3);
        assert_eq!(queue.pop(start + STALL), Some(vec![0; 5]));
        queue.wrote();
        assert!(!queue.stalled(start + STALL * 3), "progress resets it");
    }

    #[test]
    fn a_write_the_shell_never_drains_stalls_too() {
        let start = Instant::now();
        let mut queue = DataQueue::new(100);
        queue.push(Some(A), vec![0; 3]);
        assert_eq!(queue.pop(start), Some(vec![0; 3]), "credit is plenty");
        assert!(queue.stalled(start + STALL), "but the write never finished");
    }

    #[test]
    fn page_admission_is_released_only_when_its_reply_finishes_writing() {
        let mut queue = DataQueue::new(100);
        for _ in 0..capsula_protocol::message::MAX_PENDING_PAGES {
            assert!(queue.admit_page());
        }
        assert!(!queue.admit_page());
        queue.push(None, vec![1]);
        queue.push(Some(A), vec![2]);
        queue.revoke(A);
        assert_eq!(queue.pop(Instant::now()), Some(vec![1]));
        assert!(
            !queue.admit_page(),
            "a write still in progress isn't answered"
        );
        queue.wrote();
        assert!(queue.admit_page());
        assert!(!queue.admit_page());
        assert_eq!(queue.pop(Instant::now()), None);
    }
}
