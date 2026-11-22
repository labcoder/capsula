//! The threads panel's page state, independent of GPUI and the connection transport.

use std::collections::{HashMap, HashSet, VecDeque};
use std::{cell::RefCell, rc::Rc};

use kumochi_protocol::constants;
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    DataToBrain, DataToShell, DraftId, EventId, MAX_ISSUE_CARDS, MAX_PAGE_BYTES, MAX_PAGE_ROWS,
    ModelName, PageEvent, PageOf, RunId, SettingsView, ThreadCursor, ThreadId, ThreadKind,
    ThreadRow,
};
use kumochi_scene::text::Bounded;

use crate::strings::Text;

mod cards;
pub use cards::Feedback as IssueFeedback;
#[cfg(test)]
mod tests;

const HISTORY_ROWS: usize = 4 * MAX_PAGE_ROWS;
const HISTORY_TEXT_BYTES: usize = 4 * MAX_PAGE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryEdge {
    Older,
    Inside,
    Newer,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistoryAnchor {
    pub event: EventId,
    pub offset: f32,
}

#[derive(Default)]
struct HistoryWindow {
    after: Option<EventId>,
    scrolled: bool,
    refresh: Option<EventId>,
    note: Option<PendingNote>,
}

struct PendingNote {
    request: Option<u32>,
    note: ScreenNote,
}

impl HistoryWindow {
    fn next_read(&self) -> Option<Read> {
        if self
            .note
            .as_ref()
            .is_some_and(|note| note.request.is_none())
        {
            return Some(Read::NoteAnchor);
        }
        self.refresh.map(Read::WindowRefresh)
    }

    fn take_note(&mut self, request: u32) -> Option<ScreenNote> {
        if self.note.as_ref()?.request != Some(request) {
            return None;
        }
        self.note.take().map(|pending| pending.note)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum View {
    #[default]
    Browse,
    Chat,
}

#[derive(Debug, Default)]
struct Reads {
    counter: u32,
    pending: Vec<(u32, View)>,
    cards: cards::Cards,
    issue_confirmations: [Option<(DraftId, std::rc::Weak<std::cell::Cell<bool>>)>; 2],
    exhausted: bool,
}

impl Reads {
    fn admit(&mut self, view: View) -> Option<u32> {
        if self.pending.len() == kumochi_protocol::message::MAX_PENDING_PAGES {
            return None;
        }
        let Some(request) = self.counter.checked_add(1) else {
            self.exhausted = true;
            return None;
        };
        self.counter = request;
        self.pending.push((request, view));
        Some(request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    First,
    More,
    Newer,
    NoteAnchor,
    WindowRefresh(EventId),
    Repair,
    /// The newest page, replacing every cached row, so older ones reload through pagination.
    Refresh,
    Renamed(Option<ThreadCursor>),
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    request: u32,
    revision: u64,
    read: Read,
    /// A `RunState` arrived while this list read was in flight, so its reply may predate it.
    stale: bool,
    after: Option<ThreadCursor>,
    proves: Option<RunId>,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum SearchError {
    #[error("the search is too long for a data frame")]
    TooLong,
}

/// A line the shell adds to one conversation and never stores: said once when the default
/// model starts or stops taking a hold's picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenNote {
    pub thread: ThreadId,
    /// The newest stored event when it was said, which it follows; none in an empty conversation.
    pub after: Option<EventId>,
    /// Which of the two lines: `model` takes the picture, or doesn't.
    pub text: Text,
    pub model: ModelName,
}

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about the panel and its reads, not a state machine"
)]
pub struct Threads {
    pub settings: Option<SettingsView>,
    pub rows: Vec<ThreadRow>,
    pub next: Option<ThreadCursor>,
    pub open: Option<ThreadId>,
    /// Newest first; stored event ids remain distinct, even within one run.
    pub events: Vec<PageEvent>,
    pub before: Option<EventId>,
    window: Option<HistoryWindow>,
    pub listed: bool,
    pub loaded: bool,
    pub proven: Option<RunId>,
    settled: Option<RunId>,
    pub search_error: Option<SearchError>,
    /// The last read failed; cleared by the next page that lands.
    pub failed: bool,
    /// Threads whose delete is unanswered; they show as deleting and can't be chosen.
    pub deleting: Vec<ThreadId>,
    /// Every delete slot is held, so no other thread can be deleted yet.
    pub deletes_full: bool,
    /// The last delete answered that nothing changed; cleared by the next confirm.
    pub delete_failed: bool,
    /// The open conversation's one line that isn't stored; see [`Self::screen_sees`].
    screen_note: Option<ScreenNote>,
    /// The note for what the default model last did with a hold's picture; none until it has
    /// said.
    pub sees: Option<Text>,
    connected: bool,
    /// Pages are read only while the panel is shown.
    shown: bool,
    search: Option<String>,
    reads: Rc<RefCell<Reads>>,
    view: View,
    list_revision: u64,
    event_revision: u64,
    list_pending: Option<Pending>,
    event_pending: Option<Pending>,
    list_needed: Option<Read>,
    event_needed: Option<Read>,
    deferred_search: Option<u64>,
    cached: HashMap<ThreadId, ThreadRow>,
    /// Threads whose row was read on this connection.
    fresh: HashSet<ThreadId>,
    pages: HashMap<ThreadId, Option<ThreadCursor>>,
    reread: VecDeque<Option<ThreadCursor>>,
    /// Renamed tasks whose new title no page has shown yet: the cursor of the page each was on,
    /// and the title sent. Every committed-title notification rereads these pages, so a reread
    /// that raced ahead of its rename's commit still reaches the row.
    unconfirmed: HashMap<ThreadId, (Option<ThreadCursor>, String)>,
}

impl std::fmt::Debug for Threads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Threads")
            .field("rows", &self.rows.len())
            .field("open", &self.open)
            .field("events", &self.events.len())
            .finish_non_exhaustive()
    }
}

impl Threads {
    pub fn begin_issue_confirmation(&mut self, draft: DraftId, live: &Rc<std::cell::Cell<bool>>) {
        self.retire_issue_confirmation();
        self.reads.borrow_mut().issue_confirmations[match self.view {
            View::Browse => 0,
            View::Chat => 1,
        }] = Some((draft, Rc::downgrade(live)));
    }

    pub fn retire_issue_confirmation(&mut self) {
        if let Some((_, live)) = self.reads.borrow_mut().issue_confirmations[match self.view {
            View::Browse => 0,
            View::Chat => 1,
        }]
        .take()
            && let Some(live) = live.upgrade()
        {
            live.set(false);
        }
    }

    pub(crate) fn card_request(
        &mut self,
        drafts: Bounded<DraftId, MAX_ISSUE_CARDS>,
    ) -> Option<DataToBrain> {
        if !self.connected || !self.shown || self.cards_broken() {
            return None;
        }
        let mut reads = self.reads.borrow_mut();
        let request = reads.admit(self.view)?;
        reads.cards.start(request, &drafts);
        for (draft, live) in reads.issue_confirmations.iter().flatten() {
            if drafts.contains(draft)
                && let Some(live) = live.upgrade()
            {
                live.set(false);
            }
        }
        Some(DataToBrain::IssueCardsRequest { request, drafts })
    }
    pub(crate) fn chat() -> Self {
        Self {
            view: View::Chat,
            ..Self::default()
        }
    }

    pub fn pair() -> (Self, Self) {
        let browse = Self::default();
        let chat = Self {
            reads: Rc::clone(&browse.reads),
            ..Self::chat()
        };
        (browse, chat)
    }

    /// For views that walk a bounded history in both directions.
    pub fn bounded_pair() -> (Self, Self) {
        let (mut browse, mut chat) = Self::pair();
        browse.window = Some(HistoryWindow::default());
        chat.window = Some(HistoryWindow::default());
        (browse, chat)
    }

    pub fn exhausted(&self) -> bool {
        self.reads.borrow().exhausted
    }

    pub fn cards_broken(&self) -> bool {
        self.reads.borrow().cards.broken
    }

    pub fn issue(&self, draft: DraftId) -> Option<std::cell::Ref<'_, cards::Entry>> {
        if !self.connected || !self.shown || self.cards_broken() {
            return None;
        }
        std::cell::Ref::filter_map(self.reads.borrow(), |reads| {
            reads.cards.entry(self.view, draft)
        })
        .ok()
    }

    pub fn action_state(
        &mut self,
        draft: DraftId,
        version: kumochi_protocol::message::StateVersion,
        state: kumochi_protocol::message::ActionState,
        approval: Option<kumochi_protocol::message::ApprovalRev>,
        request: Option<u64>,
    ) {
        self.reads
            .borrow_mut()
            .cards
            .state(draft, version, state, approval, request);
    }

    pub fn issue_submitted(&mut self, draft: DraftId, request: Option<u64>) {
        self.reads.borrow_mut().cards.submitted(draft, request);
    }

    pub fn issue_scope(&self) -> u64 {
        self.event_revision
    }

    pub fn issue_review_started(&mut self, draft: DraftId, request: u64) {
        self.reads.borrow_mut().cards.review_started(draft, request);
    }

    pub fn issue_review(
        &mut self,
        request: u64,
        card: kumochi_protocol::message::IssueCard,
    ) -> bool {
        self.reads.borrow_mut().cards.review(request, card)
    }

    pub fn cancel_issue_review(&mut self, request: u64, draft: DraftId) {
        self.reads.borrow_mut().cards.cancel_review(request, draft);
    }

    pub fn issue_unavailable(
        &mut self,
        request: u64,
        draft: DraftId,
        kind: kumochi_protocol::message::IssueUnavailableKind,
    ) {
        self.reads
            .borrow_mut()
            .cards
            .unavailable(request, draft, kind);
    }

    pub fn issue_refused(
        &mut self,
        request: u64,
        draft: DraftId,
        version: kumochi_protocol::message::StateVersion,
        approval: Option<kumochi_protocol::message::ApprovalRev>,
        reason: kumochi_protocol::message::IssueRefused,
    ) {
        self.reads
            .borrow_mut()
            .cards
            .refused(request, draft, version, approval, reason);
    }

    fn sync_cards(&mut self) {
        let thread = self.open.filter(|_| self.connected && self.shown);
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if thread.is_some() && self.loaded {
            for event in &self.events {
                if let kumochi_protocol::message::PageEventKind::Issue { draft } = event.kind
                    && seen.insert(draft)
                {
                    ids.push(draft);
                }
            }
        }
        self.reads.borrow_mut().cards.shown(self.view, thread, ids);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn exhaust_for_tests(&mut self) {
        self.reads.borrow_mut().counter = u32::MAX;
    }

    /// `chat`, for the shell's tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn chat_for_tests() -> Self {
        Self::chat()
    }

    /// Records which note is true of `model` now, that a hold sends it the screen's picture or
    /// that it can't take one, and leaves it in `thread`'s conversation when that changed. The
    /// first answer leaves none. A change while `thread` isn't the open conversation, or before
    /// that conversation's first page has landed, leaves none and drops the old one: a note
    /// placed before its messages are known would sit above all of them.
    pub fn screen_sees(&mut self, thread: Option<ThreadId>, text: Text, model: &ModelName) -> bool {
        if self.sees.replace(text).is_none_or(|was| was == text) {
            return false;
        }
        let note = thread
            .filter(|thread| self.open == Some(*thread) && self.loaded)
            .map(|thread| ScreenNote {
                thread,
                after: self
                    .events
                    .first()
                    .filter(|_| self.window.is_none())
                    .map(|event| event.id),
                text,
                model: model.clone(),
            });
        if let Some(window) = &mut self.window {
            let changed = self.screen_note.take().is_some() || note.is_some();
            window.note = note.map(|note| PendingNote {
                request: None,
                note,
            });
            changed
        } else {
            self.screen_note = note;
            self.screen_note.is_some()
        }
    }

    pub fn visible_screen_note(&self) -> Option<&ScreenNote> {
        self.screen_note.as_ref().filter(|note| {
            self.window.is_none()
                || (self.open == Some(note.thread)
                    && note
                        .after
                        .is_some_and(|id| self.events.iter().any(|event| event.id == id)))
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_screen_note_for_tests(&mut self, note: ScreenNote) {
        self.screen_note = Some(note);
    }

    /// `apply`, for the shell's tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn apply_for_tests(&mut self, message: DataToShell) {
        self.apply(message);
    }

    /// `card_request`, for the shell's tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn card_request_for_tests(
        &mut self,
        drafts: Bounded<DraftId, MAX_ISSUE_CARDS>,
    ) -> Option<DataToBrain> {
        self.card_request(drafts)
    }

    pub fn owns_request(&self, request: u32) -> bool {
        self.reads.borrow().pending.contains(&(request, self.view))
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.open
            .and_then(|thread| self.rows.iter().position(|row| row.id == thread))
    }

    pub fn can_rename(&self, thread: ThreadId) -> bool {
        self.connected && !self.deleting.contains(&thread)
    }

    pub fn set_settings(&mut self, settings: SettingsView) {
        self.settings = Some(settings);
    }

    /// `thread`'s row as read on this connection; none if it wasn't read since connecting.
    pub fn fresh_row(&self, thread: ThreadId) -> Option<&ThreadRow> {
        self.fresh
            .contains(&thread)
            .then(|| self.row(thread))
            .flatten()
    }

    pub fn row(&self, thread: ThreadId) -> Option<&ThreadRow> {
        self.cached
            .get(&thread)
            .or_else(|| self.rows.iter().find(|row| row.id == thread))
    }

    /// A rename of `thread` to `title` was sent: its page is read again now, and again after each
    /// committed-title notification until a page shows the new title.
    pub fn renamed(&mut self, thread: ThreadId, title: &str) {
        let after = self.pages.get(&thread).copied().flatten();
        self.unconfirmed.insert(thread, (after, title.to_owned()));
        if !self.reread.contains(&after) {
            self.reread.push_back(after);
        }
    }

    pub fn online(&self) -> bool {
        self.connected
    }

    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    pub fn listing(&self) -> bool {
        self.list_pending.is_some() || self.list_needed.is_some() || !self.reread.is_empty()
    }

    pub fn reading(&self) -> bool {
        self.event_pending.is_some()
            || self.event_needed.is_some()
            || self
                .window
                .as_ref()
                .and_then(HistoryWindow::next_read)
                .is_some()
    }

    pub fn history_revision(&self) -> u64 {
        self.event_revision
    }

    pub fn connected(&mut self) {
        self.retire_issue_confirmation();
        self.connected = true;
        self.proven = None;
        self.settled = None;
        self.refresh();
        self.event_revision = self.event_revision.wrapping_add(1);
        self.event_needed = self.open.map(|_| Read::First);
        self.loaded = false;
        self.sync_cards();
    }

    /// The panel opened: reads start again, from a fresh first page.
    pub fn show(&mut self) {
        self.shown = true;
        self.refresh();
        self.sync_cards();
    }

    pub fn show_for(&mut self, thread: Option<ThreadId>) {
        self.select(thread);
        self.show();
    }

    pub fn follow(&mut self, thread: Option<ThreadId>) -> bool {
        if self.open == thread {
            return false;
        }
        self.select(thread);
        true
    }

    /// The panel closed: nothing more is read until it opens.
    pub fn hide(&mut self) {
        self.shown = false;
        self.reread.clear();
        self.unconfirmed.clear();
        self.select(None);
    }

    pub fn disconnected(&mut self) {
        self.retire_issue_confirmation();
        let mut reads = self.reads.borrow_mut();
        reads.pending.clear();
        for (_, live) in reads.issue_confirmations.iter().flatten() {
            if let Some(live) = live.upgrade() {
                live.set(false);
            }
        }
        reads.cards.clear();
        if reads.exhausted {
            reads.counter = 0;
            reads.exhausted = false;
        }
        drop(reads);
        self.fresh.clear();
        self.connected = false;
        self.proven = None;
        self.settled = None;
        self.list_pending = None;
        self.event_pending = None;
        self.list_needed = None;
        self.event_needed = None;
        if let Some(window) = &mut self.window {
            *window = HistoryWindow::default();
        }
        self.deferred_search = None;
        self.reread.clear();
        self.unconfirmed.clear();
    }

    pub fn refresh(&mut self) {
        self.reread.clear();
        self.deferred_search = None;
        self.list_revision = self.list_revision.wrapping_add(1);
        self.list_needed = self.search_error.is_none().then_some(Read::First);
    }

    pub(crate) fn search(&mut self, words: &str) {
        let search = (!words.trim().is_empty()).then(|| words.trim().to_owned());
        if self.search == search {
            return;
        }
        let candidate = DataToBrain::PageRequest {
            request: u32::MAX,
            page: PageOf::Threads {
                search: search.clone(),
                after: Some(ThreadCursor {
                    updated_ms: i64::MAX,
                    id: ThreadId(u64::MAX),
                    rank_bits: u64::MAX,
                }),
            },
        };
        self.search_error = frame::encode(&candidate, Pipe::Data)
            .is_err()
            .then_some(SearchError::TooLong);
        self.search = search;
        self.rows.clear();
        self.pages.clear();
        self.unconfirmed.clear();
        self.next = None;
        self.listed = false;
        self.refresh();
    }

    pub fn defer_search(&mut self, words: &str) -> u64 {
        self.search(words);
        self.deferred_search = Some(self.list_revision);
        self.list_revision
    }

    pub fn finish_search(&mut self, revision: u64) {
        if self.deferred_search == Some(revision) {
            self.deferred_search = None;
        }
    }

    pub fn select(&mut self, thread: Option<ThreadId>) {
        self.retire_issue_confirmation();
        self.open = thread;
        self.proven = None;
        self.settled = None;
        self.events.clear();
        self.before = None;
        if let Some(window) = &mut self.window {
            *window = HistoryWindow::default();
        }
        self.loaded = false;
        self.event_revision = self.event_revision.wrapping_add(1);
        self.event_needed = thread.map(|_| Read::First);
        self.sync_cards();
    }

    /// `thread` was hidden or removed: its row, its open messages and every read in flight
    /// about it go, so no reply sent before the brain hid it lands.
    pub fn deleted(&mut self, thread: ThreadId) {
        self.cached.remove(&thread);
        self.pages.remove(&thread);
        self.unconfirmed.remove(&thread);
        self.rows.retain(|row| row.id != thread);
        if self.shown
            && let Some(Pending {
                revision,
                read: Read::Renamed(after),
                ..
            }) = self.list_pending
            && revision == self.list_revision
            && !self.reread.contains(&after)
        {
            self.reread.push_front(after);
        }
        self.list_revision = self.list_revision.wrapping_add(1);
        if self.list_pending.is_some() && self.list_needed.is_none() {
            self.list_needed = Some(Read::Repair);
        }
        if self.open == Some(thread) {
            self.select(None);
        }
    }

    pub fn more_threads(&mut self) {
        if self.next.is_some() && !self.listing() {
            self.list_needed = Some(Read::More);
        }
    }

    pub fn older_events(&mut self) {
        if self.before.is_some() && !self.reading() {
            self.event_needed = Some(Read::More);
        }
    }

    pub fn has_newer_events(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.after.is_some())
    }

    pub fn history_scrolled(&mut self, edge: HistoryEdge) -> bool {
        let Some(window) = &mut self.window else {
            return false;
        };
        window.scrolled = edge != HistoryEdge::Newer || window.after.is_some();
        if self.reading() {
            return false;
        }
        match edge {
            HistoryEdge::Older => self.older_events(),
            HistoryEdge::Newer if self.has_newer_events() => {
                self.event_needed = Some(Read::Newer);
            }
            HistoryEdge::Newer | HistoryEdge::Inside => {}
        }
        self.reading()
    }

    /// The view supplies a real visible event and its offset, never an edge sentinel.
    pub fn history_anchor(&self, anchor: HistoryAnchor) -> Option<HistoryAnchor> {
        self.events
            .iter()
            .any(|event| event.id == anchor.event)
            .then_some(anchor)
    }

    pub fn settled(&mut self, thread: ThreadId, run: RunId) {
        if self.open == Some(thread) {
            self.settled = Some(run);
        }
    }

    /// `thread`'s stored events changed with no run frame: its open page is read again, and the
    /// reply replaces every cached row, so none stays as it was before the change.
    pub fn events_changed(&mut self, thread: ThreadId) {
        if self.open == Some(thread) && self.event_needed != Some(Read::First) {
            self.event_needed = Some(Read::Refresh);
        }
    }

    pub fn run_state(&mut self, thread: ThreadId) {
        // shortcut: Stop and failure are told before their outcome is written, so this read can
        // miss it until the next RunState; refresh on the store's ack if that shows.
        if self.open == Some(thread)
            && !matches!(self.event_needed, Some(Read::First | Read::Refresh))
        {
            self.event_needed = Some(Read::Repair);
        }
        if self.searching() {
            return;
        }
        // The read in flight may predate this change, so it earns one more read when it lands.
        if let Some(pending) = &mut self.list_pending {
            pending.stale = true;
            return;
        }
        if self.list_needed.is_none() && self.rows.first().is_none_or(|row| row.id != thread) {
            self.list_needed = Some(Read::Repair);
        }
    }

    /// The brain committed a thread's title: the list is read again, or the read in flight earns
    /// one more, and so is the page of every rename no page has confirmed, since its earlier
    /// reread may have beaten the commit.
    pub fn changed(&mut self) {
        if self.searching() {
            return;
        }
        for (after, _) in self.unconfirmed.values() {
            if !self.reread.contains(after) {
                self.reread.push_back(*after);
            }
        }
        if let Some(pending) = &mut self.list_pending {
            pending.stale = true;
            return;
        }
        if self.list_needed.is_none() {
            self.list_needed = Some(Read::Repair);
        }
    }

    /// Marks requests in flight before handing them to the nonblocking sender. A failed send
    /// disconnects this state; changing a query or selection never frees an unanswered slot.
    pub fn requests(&mut self) -> Vec<DataToBrain> {
        if !self.connected || !self.shown || self.cards_broken() {
            return Vec::new();
        }
        if self.reads.borrow_mut().cards.take_refresh(self.view)
            && self.open.is_some()
            && !matches!(self.event_needed, Some(Read::First | Read::Refresh))
        {
            self.event_needed = Some(Read::Repair);
        }
        let mut requests = Vec::new();
        if self.list_pending.is_none()
            && self.deferred_search.is_none()
            && let Some(read) = self
                .reread
                .front()
                .copied()
                .map(Read::Renamed)
                .or(self.list_needed)
            && let Some(request) = self.reads.borrow_mut().admit(self.view)
        {
            if matches!(read, Read::Renamed(_)) {
                self.reread.pop_front();
            } else {
                self.list_needed = None;
            }
            let after = match read {
                Read::More => self.next,
                Read::Renamed(after) => after,
                Read::First
                | Read::Repair
                | Read::Refresh
                | Read::Newer
                | Read::NoteAnchor
                | Read::WindowRefresh(_) => None,
            };
            self.list_pending = Some(Pending {
                request,
                revision: self.list_revision,
                read,
                stale: false,
                after,
                proves: None,
            });
            requests.push(DataToBrain::PageRequest {
                request,
                page: PageOf::Threads {
                    search: self.search.clone(),
                    after,
                },
            });
        }
        requests.extend(self.request_event_page());
        loop {
            let ids = self.reads.borrow().cards.wanted(self.view);
            if ids.is_empty() {
                break;
            }
            let drafts = Bounded::new(ids).expect("card selection is bounded to sixteen");
            let Some(request) = self.card_request(drafts) else {
                break;
            };
            requests.push(request);
        }
        requests
    }

    fn request_event_page(&mut self) -> Option<DataToBrain> {
        if self.event_pending.is_some() {
            return None;
        }
        let thread = self.open?;
        let read = self
            .event_needed
            .or_else(|| self.window.as_ref().and_then(HistoryWindow::next_read))?;
        let request = self.reads.borrow_mut().admit(self.view)?;
        self.event_needed = None;
        if let Some(window) = &mut self.window {
            match read {
                Read::NoteAnchor => {
                    if let Some(note) = &mut window.note {
                        note.request = Some(request);
                    }
                }
                Read::WindowRefresh(_) => window.refresh = None,
                _ => {}
            }
        }
        self.event_pending = Some(Pending {
            request,
            revision: self.event_revision,
            read,
            stale: false,
            after: None,
            proves: if matches!(
                read,
                Read::First | Read::Repair | Read::Refresh | Read::Newer
            ) {
                self.settled
            } else {
                None
            },
        });
        Some(DataToBrain::PageRequest {
            request,
            page: match read {
                Read::Newer => PageOf::EventsAfter {
                    thread,
                    after: self
                        .window
                        .as_ref()
                        .and_then(|window| window.after)
                        .expect("a newer read has a cursor"),
                },
                Read::WindowRefresh(before) => PageOf::Events {
                    thread,
                    before: Some(before),
                },
                _ => PageOf::Events {
                    thread,
                    before: self.before.filter(|_| read == Read::More),
                },
            },
        })
    }

    /// Applies a page frame and returns its credit: the whole frame, used or dropped, since the
    /// brain counted all of it against the window.
    pub fn receive(&mut self, message: DataToShell, bytes: u32) -> u32 {
        self.apply(message);
        bytes
    }

    pub(crate) fn apply(&mut self, message: DataToShell) {
        if let DataToShell::IssueCardRead { request, card } = message {
            self.reads.borrow_mut().cards.full(request, card);
            return;
        }
        if let DataToShell::IssueCardsDone { request, .. } = &message {
            let mut reads = self.reads.borrow_mut();
            if reads.pending.contains(&(*request, self.view)) && reads.cards.pending(*request) {
                reads.pending.retain(|(id, _)| id != request);
                reads.cards.done(*request);
            }
            return;
        }
        if let DataToShell::ThreadList { request, .. }
        | DataToShell::Page { request, .. }
        | DataToShell::PageFailed { request } = &message
        {
            let pending = match &message {
                DataToShell::ThreadList { .. } => self.list_pending,
                DataToShell::Page { .. } => self.event_pending,
                DataToShell::PageFailed { .. } => self
                    .list_pending
                    .filter(|pending| pending.request == *request)
                    .or(self.event_pending),
                _ => None,
            };
            if !self.owns_request(*request)
                || pending.is_none_or(|pending| pending.request != *request)
            {
                return;
            }
            self.reads
                .borrow_mut()
                .pending
                .retain(|(id, _)| id != request);
        }
        match message {
            DataToShell::ThreadList {
                request,
                threads,
                next,
            } => self.apply_list(request, threads, next),
            DataToShell::Page {
                request,
                thread,
                events,
                next,
            } => {
                let Some(pending) = self.event_pending.take() else {
                    return;
                };
                if pending.revision != self.event_revision || self.open != Some(thread) {
                    return;
                }
                if pending.read == Read::NoteAnchor {
                    if let Some(mut note) = self
                        .window
                        .as_mut()
                        .and_then(|window| window.take_note(request))
                    {
                        note.after = events.first().map(|event| event.id);
                        self.screen_note = Some(note);
                    }
                    return;
                }
                self.apply_events(pending.read, events, next);
                self.loaded = true;
                self.failed = false;
                if pending.proves.is_some() && !self.has_newer_events() {
                    self.proven = pending.proves;
                }
            }
            DataToShell::PageFailed { request } => self.read_failed(request),
            DataToShell::IssueCard(_)
            | DataToShell::IssueCardRead { .. }
            | DataToShell::IssueCardsDone { .. }
            | DataToShell::IssueReview { .. }
            | DataToShell::AssistantText { .. }
            | DataToShell::StepCommit { .. }
            | DataToShell::RunEnd { .. }
            | DataToShell::SceneSnapshot { .. }
            | DataToShell::ScenesRemoved { .. }
            | DataToShell::ScenesReplaced { .. }
            | DataToShell::SceneBegin { .. }
            | DataToShell::SceneStepCommit { .. }
            | DataToShell::SceneSnapshotV1 { .. }
            | DataToShell::SceneCutApplied { .. }
            | DataToShell::SceneEnd { .. }
            | DataToShell::Models { .. }
            | DataToShell::HoldWords { .. }
            | DataToShell::HoldStored { .. }
            | DataToShell::EventsChanged { .. }
            | DataToShell::Transcript(_)
            | DataToShell::SegmentQueued { .. }
            | DataToShell::SpeechPcm(_) => {
                // shortcut: Presence consumes v1 frames through Presenter first; B3 removes placeholder routing.
            }
        }
        self.sync_cards();
    }

    fn apply_events(&mut self, read: Read, events: Vec<PageEvent>, next: Option<EventId>) {
        let newest = self.events.first().map(|event| event.id);
        let away = newest.is_some()
            && self
                .window
                .as_ref()
                .is_some_and(|window| window.scrolled || window.after.is_some());
        let refresh = if read == Read::Refresh {
            next.zip(newest)
                .map(|(cursor, newest)| cursor.min(EventId(newest.0.saturating_add(1))))
        } else if matches!(read, Read::WindowRefresh(_)) {
            next
        } else {
            None
        };
        match read {
            Read::First => self.replace_events(events, next),
            Read::Refresh if self.window.is_none() => self.replace_events(events, next),
            Read::More if self.window.is_none() && !self.loaded => {
                self.replace_events(events, next);
            }
            Read::Repair | Read::Refresh
                if !away
                    && (!self.loaded
                        || next.is_some()
                            && newest
                                .is_none_or(|id| events.iter().all(|event| event.id > id))) =>
            {
                self.replace_events(events, next);
            }
            Read::Repair | Read::Refresh => {
                let mut fresh = Vec::new();
                for event in events {
                    if newest.is_none_or(|id| event.id > id) {
                        if away {
                            if let Some(window) = &mut self.window {
                                window.after = newest;
                            }
                        } else {
                            fresh.push(event);
                        }
                    } else if let Some(shown) =
                        self.events.iter_mut().find(|shown| shown.id == event.id)
                    {
                        shown.kind = event.kind;
                    }
                }
                self.events.splice(0..0, fresh);
                self.trim_events(HistoryEdge::Newer);
            }
            Read::WindowRefresh(_) => {
                for event in events {
                    if let Some(shown) = self.events.iter_mut().find(|shown| shown.id == event.id) {
                        shown.kind = event.kind;
                    }
                }
            }
            Read::More => {
                for event in events {
                    if !self.events.iter().any(|old| old.id == event.id) {
                        self.events.push(event);
                    }
                }
                self.before = next;
                self.trim_events(HistoryEdge::Older);
            }
            Read::Newer => {
                self.events.splice(0..0, events);
                if let Some(window) = &mut self.window {
                    window.after = next;
                }
                self.trim_events(HistoryEdge::Newer);
            }
            Read::NoteAnchor | Read::Renamed(_) => unreachable!("not an event window read"),
        }
        if matches!(read, Read::Refresh | Read::WindowRefresh(_))
            && let Some(window) = &mut self.window
        {
            window.refresh = refresh
                .filter(|before| self.events.last().is_some_and(|oldest| *before > oldest.id));
        }
    }

    fn replace_events(&mut self, events: Vec<PageEvent>, next: Option<EventId>) {
        self.events = events;
        self.before = next;
        if let Some(window) = &mut self.window {
            window.after = None;
        }
        self.trim_events(HistoryEdge::Newer);
    }

    fn trim_events(&mut self, toward: HistoryEdge) {
        let Some(window) = &mut self.window else {
            return;
        };
        let older = toward == HistoryEdge::Older;
        let mut retained = 0;
        let mut bytes = 0;
        let len = self.events.len();
        while retained < len.min(HISTORY_ROWS) {
            let index = if older { len - 1 - retained } else { retained };
            bytes += self.events[index].text.len();
            if bytes > HISTORY_TEXT_BYTES {
                break;
            }
            retained += 1;
        }
        if retained < len {
            if older {
                self.events.drain(..len - retained);
                window.after = self.events.first().map(|event| event.id);
            } else {
                self.events.truncate(retained);
                self.before = self.events.last().map(|event| event.id);
            }
        }
    }

    fn apply_list(&mut self, request: u32, threads: Vec<ThreadRow>, next: Option<ThreadCursor>) {
        let Some(pending) = self
            .list_pending
            .filter(|pending| pending.request == request)
        else {
            return;
        };
        self.list_pending = None;
        if pending.stale && self.list_needed.is_none() {
            self.list_needed = Some(Read::Repair);
        }
        if pending.revision != self.list_revision {
            return;
        }
        if pending.read == Read::First {
            self.pages.clear();
        }
        for row in &threads {
            if self
                .unconfirmed
                .get(&row.id)
                .is_some_and(|(_, title)| *title == row.title)
            {
                self.unconfirmed.remove(&row.id);
            }
            self.cached.insert(row.id, row.clone());
            self.fresh.insert(row.id);
            self.pages.insert(row.id, pending.after);
        }
        match pending.read {
            Read::First | Read::Refresh => {
                self.rows = threads;
                self.next = next;
            }
            Read::More => {
                for row in threads {
                    if !self.rows.iter().any(|old| old.id == row.id) {
                        self.rows.push(row);
                    }
                }
                self.next = next;
            }
            Read::Repair => {
                if !self.listed {
                    self.next = next;
                }
                // shortcut: over 50 changed threads can remain out of order until reopen;
                // revisit when more than one user turn can update many threads at once.
                self.rows
                    .retain(|old| !threads.iter().any(|row| row.id == old.id));
                self.rows.splice(0..0, threads);
            }
            Read::Newer | Read::NoteAnchor | Read::WindowRefresh(_) => {
                unreachable!("a history read cannot return tasks")
            }
            Read::Renamed(_) => {
                for row in threads {
                    if let Some(old) = self.rows.iter_mut().find(|old| old.id == row.id) {
                        *old = row;
                    }
                }
            }
        }
        self.listed = true;
        self.failed = false;
    }

    /// Frees the failed request's slot, and shows the failure only when the request was still
    /// current. No retry here: the next refresh, selection or `RunState` reads again.
    fn read_failed(&mut self, request: u32) {
        let current = if let Some(pending) = self
            .list_pending
            .filter(|pending| pending.request == request)
        {
            self.list_pending = None;
            pending.revision == self.list_revision
        } else if let Some(pending) = self
            .event_pending
            .filter(|pending| pending.request == request)
        {
            self.event_pending = None;
            let current = pending.revision == self.event_revision;
            if pending.read == Read::NoteAnchor {
                if current && let Some(window) = &mut self.window {
                    drop(window.take_note(request));
                }
                return;
            }
            if current {
                self.event_revision = self.event_revision.wrapping_add(1);
            }
            current
        } else {
            return;
        };
        self.failed |= current;
    }
}

pub fn row_label(row: &ThreadRow, settings: Option<&SettingsView>) -> String {
    if row.kind == ThreadKind::Main {
        settings
            .map_or(constants::DEFAULT_ASSISTANT_NAME, |settings| {
                settings.assistant_name.as_str()
            })
            .to_owned()
    } else {
        row.title.clone()
    }
}
