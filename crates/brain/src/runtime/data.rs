//! The data pipe's outgoing queue: frames in order, sent only against the byte credit the shell
//! grants, so a shell that stops reading can't make the brain send without bound.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::coordinator::{CardRead, Cards, PageKey};
use crate::recall::control::{CancelOnDrop, Control, READ_TIME};
use kumochi_protocol::message::{
    DataToShell, DraftId, IssueCardsStatus, MAX_ISSUE_CARDS, MAX_PENDING_PAGES, RunId, SceneId,
    ThreadId,
};
use kumochi_scene::text::Bounded;

#[cfg(test)]
#[path = "card_queue_tests.rs"]
mod card_tests;

/// How long the shell may leave the queue blocked on credit before the run ends as
/// `view_stalled` (architecture v1 §3.2) [guess].
pub(crate) const STALL: Duration = Duration::from_secs(2);
/// What a queued frame costs beyond its bytes.
const ENTRY: usize = std::mem::size_of::<(Owner, Vec<u8>)>();

/// What a queued frame belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// A run's output, which a Stop or a replacement drops.
    Run(RunId),
    /// A page reply, which counts against the unanswered page requests until it's written.
    Page(u64),
    /// One card's snapshot owner, distinct from its batch terminal.
    Card {
        token: u64,
        draft: DraftId,
        thread: ThreadId,
        generation: crate::store::Generation,
    },
    /// Only this frame releases a card batch.
    CardsDone(u64),
    /// A scene's snapshot or v1 live frame: Stop never drops it, but a scene clear does.
    Scene(SceneId),
    /// A durable promise that must accompany its queued announcement through replacement.
    SpeechPromise(SceneId),
    /// A run-owned announcement whose scene replacement marker must precede it if still queued.
    SpeechAnnouncement(RunId, SceneId),
    /// A scene removal or replacement; presentation boundaries keep their original order.
    Presentation,
    /// A frame nothing revokes, such as the model catalog.
    Marker,
}

pub(crate) struct DataQueue {
    frames: VecDeque<(Owner, Vec<u8>)>,
    pages: Vec<Admitted>,
    token: u64,
    writing: Option<Owner>,
    /// The memory the queued frames hold.
    bytes: usize,
    credit: usize,
    window: usize,
    /// Since when bytes have waited, on credit or on a write the shell isn't draining.
    waiting_since: Option<Instant>,
}

impl std::fmt::Debug for DataQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataQueue")
            .field("frames", &self.frames.len())
            .field("pages", &self.pages.len())
            .field("bytes", &self.bytes)
            .field("credit", &self.credit)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Admitted {
    key: PageKey,
    _owner: CancelOnDrop,
    queued: bool,
    cards: Option<Arc<Cards>>,
}

impl DataQueue {
    /// A queue that may send `window` bytes before the first grant.
    pub(crate) fn new(window: usize) -> Self {
        let credit = window;
        #[cfg(all(windows, debug_assertions, feature = "test-support"))]
        let credit = super::audio_fixture::probe()
            .and_then(|probe| probe.initial_credit())
            .map_or(credit, |bytes| {
                usize::try_from(bytes).unwrap_or(credit).min(credit)
            });
        Self {
            frames: VecDeque::new(),
            pages: Vec::new(),
            token: 0,
            writing: None,
            bytes: 0,
            credit,
            window,
            waiting_since: None,
        }
    }

    #[cfg(all(windows, debug_assertions, feature = "test-support"))]
    pub(super) fn hold_fixture_credit(&mut self) {
        self.credit = 0;
    }

    pub(crate) fn push(&mut self, owner: Owner, frame: Vec<u8>) {
        self.bytes += frame.len() + ENTRY;
        self.frames.push_back((owner, frame));
    }

    pub(crate) fn push_review(&mut self, frame: Vec<u8>) -> Result<(), super::Full> {
        if frame.len().saturating_add(ENTRY) > super::DATA_CAP.saturating_sub(self.bytes) {
            return Err(super::Full);
        }
        self.push(Owner::Marker, frame);
        Ok(())
    }

    pub(crate) fn admit_page(&mut self, epoch: u64, request: u32) -> Option<PageKey> {
        if self.pages.len() == MAX_PENDING_PAGES
            || self.pages.iter().any(|page| page.key.request == request)
        {
            return None;
        }
        self.token = self.token.checked_add(1)?;
        let control = Control::new(Instant::now() + READ_TIME);
        let key = PageKey {
            epoch,
            request,
            token: self.token,
            control: control.clone(),
        };
        self.pages.push(Admitted {
            key: key.clone(),
            _owner: CancelOnDrop(control),
            queued: false,
            cards: None,
        });
        Some(key)
    }

    pub(crate) fn accepts_page(&self, key: &PageKey) -> bool {
        self.pages.iter().any(|page| {
            page.cards.is_none()
                && page.key == *key
                && !page.queued
                && page.key.control.check().is_ok()
        })
    }

    pub(crate) fn admit_cards(
        &mut self,
        epoch: u64,
        request: u32,
        drafts: &Bounded<DraftId, MAX_ISSUE_CARDS>,
    ) -> Option<Arc<Cards>> {
        let key = self.admit_page(epoch, request)?;
        let cards = Arc::new(Cards::new(key, drafts));
        self.pages.last_mut()?.cards = Some(Arc::clone(&cards));
        Some(cards)
    }

    pub(crate) fn accepts_cards(&self, cards: &Cards) -> bool {
        self.pages.iter().any(|page| {
            page.key == cards.key
                && !page.queued
                && page.key.control.check().is_ok()
                && page
                    .cards
                    .as_ref()
                    .is_some_and(|owned| std::ptr::eq(owned.as_ref(), cards))
        })
    }

    pub(crate) fn push_cards(&mut self, cards: &Cards, reply: Result<Vec<CardRead>, ()>) -> bool {
        if !self.accepts_cards(cards) {
            return true;
        }
        if reply
            .as_ref()
            .is_ok_and(|rows| rows.len() > cards.sources.len())
        {
            return false;
        }
        let request = cards.key.request;
        let status = if reply.is_ok() {
            IssueCardsStatus::Complete
        } else {
            IssueCardsStatus::Failed
        };
        let mut frames = Vec::new();
        for read in reply.unwrap_or_default() {
            if !read.live() {
                continue;
            }
            let owner = Owner::Card {
                token: cards.key.token,
                draft: read.card.draft,
                thread: read.card.thread,
                generation: read.generation,
            };
            let Ok(frame) = kumochi_protocol::frame::encode(
                &DataToShell::IssueCardRead {
                    request,
                    card: read.card,
                },
                kumochi_protocol::frame::Pipe::Data,
            ) else {
                return false;
            };
            frames.push((owner, frame));
        }
        let Ok(terminal) = kumochi_protocol::frame::encode(
            &DataToShell::IssueCardsDone { request, status },
            kumochi_protocol::frame::Pipe::Data,
        ) else {
            return false;
        };
        self.pages
            .iter_mut()
            .find(|page| page.key == cards.key)
            .expect("the admitted card batch exists")
            .queued = true;
        for (owner, frame) in frames {
            self.push(owner, frame);
        }
        self.push(Owner::CardsDone(cards.key.token), terminal);
        true
    }

    fn live_card(&self, owner: Owner) -> bool {
        let Owner::Card {
            token,
            draft,
            thread,
            generation,
        } = owner
        else {
            return true;
        };
        self.pages
            .iter()
            .filter_map(|page| page.cards.as_ref())
            .find(|cards| cards.key.token == token)
            .and_then(|cards| cards.sources.iter().find(|source| source.draft == draft))
            .is_some_and(|source| source.live(thread, generation))
    }

    pub(crate) fn push_page(&mut self, key: &PageKey, frame: Vec<u8>) {
        if !self.accepts_page(key) {
            return;
        }
        self.pages
            .iter_mut()
            .find(|page| page.key == *key)
            .expect("the admitted page exists")
            .queued = true;
        self.push(Owner::Page(key.token), frame);
    }

    pub(crate) fn reads_ended(&self) -> bool {
        self.writing.is_some_and(|owner| !self.live_card(owner))
            || self
                .pages
                .iter()
                .any(|page| page.key.control.check().is_err())
    }

    pub(crate) fn end_reads(&mut self) {
        self.pages.clear();
    }

    pub(crate) fn has_reads(&self) -> bool {
        !self.pages.is_empty()
    }

    /// The memory the queued frames hold, for pausing what feeds the queue.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(all(windows, debug_assertions, feature = "test-support"))]
    pub(super) fn credit_blocked(&self) -> bool {
        self.frames
            .front()
            .is_some_and(|(_, frame)| frame.len() > self.credit)
    }

    #[cfg(all(windows, debug_assertions, feature = "test-support"))]
    pub(super) fn granted_credit(&self) -> usize {
        self.credit
    }

    #[cfg(test)]
    pub(super) fn frames_for_tests(&self) -> usize {
        self.frames.len()
    }

    /// Drops a stopped run's frames that haven't started writing; the shell would drop them.
    pub(crate) fn revoke(&mut self, run: RunId) {
        self.retain(|owner| !matches!(owner, Owner::Run(id) | Owner::SpeechAnnouncement(id, _) if *id == run));
    }

    /// Drops the snapshot chunks of cleared scenes that haven't started writing.
    pub(crate) fn revoke_scenes(&mut self, scenes: &[SceneId]) {
        self.retain(|owner| !matches!(owner, Owner::Scene(scene) | Owner::SpeechPromise(scene) if scenes.contains(scene)));
    }

    pub(crate) fn replace_scenes(&mut self, victims: &[SceneId], frame: Vec<u8>) {
        self.retain(|owner| !matches!(owner, Owner::Scene(scene) if victims.contains(scene)));
        let at = self
            .frames
            .iter()
            .position(|(owner, _)| match owner {
                Owner::SpeechPromise(scene) | Owner::SpeechAnnouncement(_, scene) => {
                    victims.contains(scene)
                }
                _ => false,
            })
            .unwrap_or(self.frames.len());
        let mut suffix = self.frames.split_off(at);
        for _ in 0..suffix.len() {
            let entry = suffix.pop_front().expect("one entry per unsorted slot");
            if entry.0 == Owner::Presentation {
                self.frames.push_back(entry);
            } else {
                suffix.push_back(entry);
            }
        }
        self.push(Owner::Presentation, frame);
        self.frames.append(&mut suffix);
    }

    fn retain(&mut self, keep: impl Fn(&Owner) -> bool) {
        self.frames.retain(|(owner, _)| keep(owner));
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
        while self
            .frames
            .front()
            .is_some_and(|(owner, _)| !self.live_card(*owner))
        {
            let (_, frame) = self.frames.pop_front()?;
            self.bytes -= frame.len() + ENTRY;
        }
        let len = self.frames.front()?.1.len();
        self.waiting_since.get_or_insert(now);
        if len > self.credit {
            return None;
        }
        self.credit -= len;
        self.bytes -= len + ENTRY;
        self.writing = self.frames.front().map(|(owner, _)| *owner);
        self.frames.pop_front().map(|(_, frame)| frame)
    }

    /// The frame from the last [`DataQueue::pop`] reached the pipe: progress.
    pub(crate) fn wrote(&mut self) {
        if let Some(Owner::Page(token) | Owner::CardsDone(token)) = self.writing.take() {
            self.pages.retain(|page| page.key.token != token);
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

    use kumochi_protocol::message::{RunId, SceneId};

    use super::{DataQueue, ENTRY, Owner, STALL};

    const A: Owner = Owner::Run(RunId(1));
    const B: Owner = Owner::Run(RunId(2));

    #[test]
    fn review_frames_include_entry_cost_and_never_exceed_the_data_cap() {
        let cap = crate::runtime::DATA_CAP;
        let mut queue = DataQueue::new(cap);
        assert!(matches!(
            queue.push_review(vec![0; cap - ENTRY + 1]),
            Err(crate::runtime::Full)
        ));
        assert_eq!(queue.bytes(), 0);
        queue.push_review(vec![0; cap - ENTRY]).unwrap();
        assert_eq!(queue.bytes(), cap);
        assert!(matches!(
            queue.push_review(Vec::new()),
            Err(crate::runtime::Full)
        ));
        assert_eq!(queue.bytes(), cap);
        assert_eq!(queue.pop(Instant::now()).unwrap().len(), cap - ENTRY);
        queue.wrote();
        assert_eq!(queue.bytes(), 0);
        queue.push_review(vec![0]).unwrap();
        assert_eq!(queue.bytes(), ENTRY + 1);
    }

    #[test]
    fn frames_go_in_order_and_only_against_credit() {
        let start = Instant::now();
        let mut queue = DataQueue::new(10);
        queue.push(A, vec![1; 6]);
        queue.push(A, vec![2; 6]);
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
        queue.push(A, vec![0; 9]);
        assert_eq!(queue.pop(Instant::now()), None);
    }

    #[test]
    fn a_stopped_run_loses_only_its_unsent_frames() {
        let start = Instant::now();
        let mut queue = DataQueue::new(100);
        queue.push(A, vec![1]);
        queue.push(B, vec![2]);
        queue.push(A, vec![3]);
        queue.revoke(RunId(1));
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
        queue.push(A, vec![0; 3]);
        queue.push(A, vec![0; 5]);
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
        queue.push(A, vec![0; 3]);
        assert_eq!(queue.pop(start), Some(vec![0; 3]), "credit is plenty");
        assert!(queue.stalled(start + STALL), "but the write never finished");
    }

    #[test]
    fn page_admission_is_released_only_when_its_reply_finishes_writing() {
        let mut queue = DataQueue::new(100);
        for request in
            0..u32::try_from(kumochi_protocol::message::MAX_PENDING_PAGES).expect("small")
        {
            assert!(queue.admit_page(0, request).is_some());
        }
        assert!(queue.admit_page(0, 9).is_none());
        queue.push(Owner::Page(1), vec![1]);
        queue.push(A, vec![2]);
        queue.revoke(RunId(1));
        assert_eq!(queue.pop(Instant::now()), Some(vec![1]));
        assert!(
            queue.admit_page(0, 9).is_none(),
            "a write still in progress isn't answered"
        );
        queue.wrote();
        assert!(queue.admit_page(0, 9).is_some());
        assert!(queue.admit_page(0, 9).is_none());
        assert_eq!(queue.pop(Instant::now()), None);
    }

    #[test]
    fn page_identity_collision_exhaustion_and_terminal_ownership_fail_closed() {
        let mut queue = DataQueue::new(100);
        let admitted = Instant::now();
        let key = queue.admit_page(8, 7).unwrap();
        assert_eq!(queue.admit_page(8, 7), None);
        assert!(key.control.end >= admitted + super::READ_TIME);
        assert!(key.control.end <= Instant::now() + super::READ_TIME);
        let mut stale = key.clone();
        stale.epoch = 7;
        assert!(!queue.accepts_page(&stale));
        stale = key.clone();
        stale.token += 1;
        assert!(!queue.accepts_page(&stale));
        queue.push_page(&key, vec![1]);
        queue.push_page(&key, vec![2]);
        assert_eq!(queue.frames_for_tests(), 1);
        assert_eq!(queue.pop(Instant::now()), Some(vec![1]));
        assert_eq!(key.control.check(), Ok(()));
        assert_eq!(queue.admit_page(8, 7), None);
        queue.wrote();
        assert_eq!(key.control.check(), Err(crate::recall::Error::Ended));
        assert_eq!(queue.pages.len(), 0);
        queue.wrote();
        assert_eq!(queue.pages.len(), 0);
        queue.token = u64::MAX;
        assert_eq!(queue.admit_page(8, 9), None);
        assert_eq!(queue.admit_page(8, 10), None);
    }

    #[test]
    fn cancelled_pages_cannot_revive_and_session_teardown_cancels_every_owner() {
        let mut queue = DataQueue::new(100);
        let first = queue.admit_page(3, 1).unwrap();
        let second = queue.admit_page(3, 2).unwrap();
        first.control.cancel();
        queue.push_page(&first, vec![1]);
        assert_eq!(queue.frames_for_tests(), 0);
        assert!(queue.reads_ended());
        queue.end_reads();
        assert_eq!(second.control.check(), Err(crate::recall::Error::Ended));
        let current = queue.admit_page(4, 1).unwrap();
        queue.push_page(&first, vec![2]);
        assert!(queue.accepts_page(&current));
        assert_eq!(queue.frames_for_tests(), 0);
        drop(queue);
        assert_eq!(current.control.check(), Err(crate::recall::Error::Ended));
    }

    #[test]
    fn a_cleared_scene_s_chunks_leave_and_everything_else_stays_in_order() {
        let mut queue = DataQueue::new(100);
        queue.push(Owner::Scene(SceneId(1)), vec![1; 3]);
        queue.push(Owner::Scene(SceneId(2)), vec![2]);
        queue.push(A, vec![3]);
        queue.push(Owner::Page(1), vec![4]);
        queue.push(Owner::Scene(SceneId(1)), vec![5]);
        queue.revoke_scenes(&[SceneId(1), SceneId(9)]);
        assert_eq!(queue.bytes(), 3 * (1 + ENTRY));
        let sent: Vec<_> = std::iter::from_fn(|| queue.pop(Instant::now())).collect();
        assert_eq!(sent, [vec![2], vec![3], vec![4]]);
    }

    #[test]
    fn a_snapshot_chunk_is_never_revoked_and_never_answers_a_page() {
        let mut queue = DataQueue::new(100);
        assert!(queue.admit_page(0, 9).is_some());
        queue.push(Owner::Scene(SceneId(1)), vec![1]);
        queue.push(A, vec![2]);
        queue.revoke(RunId(1));
        assert_eq!(queue.pop(Instant::now()), Some(vec![1]));
        queue.wrote();
        for request in
            1..u32::try_from(kumochi_protocol::message::MAX_PENDING_PAGES).expect("small")
        {
            assert!(queue.admit_page(0, request).is_some());
        }
        assert!(
            queue.admit_page(0, 9).is_none(),
            "the chunk didn't release the page's slot"
        );
        assert_eq!(
            queue.pop(Instant::now()),
            None,
            "the run's frame was revoked"
        );
    }
}
