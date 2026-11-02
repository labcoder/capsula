//! The threads panel's page state, independent of GPUI and the connection transport.

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    DataToBrain, DataToShell, EventId, PageEvent, PageOf, ThreadCursor, ThreadId, ThreadRow,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    First,
    More,
    Repair,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    request: u32,
    revision: u64,
    read: Read,
    /// A `RunState` arrived while this list read was in flight, so its reply may predate it.
    stale: bool,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub(crate) enum SearchError {
    #[error("the search is too long for a data frame")]
    TooLong,
}

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about the panel and its reads, not a state machine"
)]
pub(crate) struct Threads {
    pub(crate) rows: Vec<ThreadRow>,
    pub(crate) next: Option<ThreadCursor>,
    pub(crate) open: Option<ThreadId>,
    /// Newest first; the view reverses their indices rather than copying their text.
    pub(crate) events: Vec<PageEvent>,
    pub(crate) before: Option<EventId>,
    pub(crate) listed: bool,
    pub(crate) loaded: bool,
    pub(crate) search_error: Option<SearchError>,
    /// The last read failed; cleared by the next page that lands.
    pub(crate) failed: bool,
    /// Threads whose delete is unanswered; they show as deleting and can't be chosen.
    pub(crate) deleting: Vec<ThreadId>,
    /// Every delete slot is held, so no other thread can be deleted yet.
    pub(crate) deletes_full: bool,
    /// The last delete answered that nothing changed; cleared by the next confirm.
    pub(crate) delete_failed: bool,
    connected: bool,
    /// Pages are read only while the panel is shown.
    shown: bool,
    search: Option<String>,
    counter: u32,
    list_revision: u64,
    event_revision: u64,
    list_pending: Option<Pending>,
    event_pending: Option<Pending>,
    list_needed: Option<Read>,
    event_needed: Option<Read>,
    deferred_search: Option<u64>,
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
    pub(crate) fn online(&self) -> bool {
        self.connected
    }

    pub(crate) fn searching(&self) -> bool {
        self.search.is_some()
    }

    pub(crate) fn listing(&self) -> bool {
        self.list_pending.is_some() || self.list_needed.is_some()
    }

    pub(crate) fn reading(&self) -> bool {
        self.event_pending.is_some() || self.event_needed.is_some()
    }

    pub(crate) fn connected(&mut self) {
        self.connected = true;
        self.refresh();
        self.event_revision = self.event_revision.wrapping_add(1);
        self.event_needed = self.open.map(|_| Read::First);
        self.loaded = false;
    }

    /// The panel opened: reads start again, from a fresh first page.
    pub(crate) fn show(&mut self) {
        self.shown = true;
        self.refresh();
    }

    /// The panel closed: nothing more is read until it opens.
    pub(crate) fn hide(&mut self) {
        self.shown = false;
        self.select(None);
    }

    pub(crate) fn disconnected(&mut self) {
        self.connected = false;
        self.list_pending = None;
        self.event_pending = None;
        self.list_needed = None;
        self.event_needed = None;
        self.deferred_search = None;
    }

    pub(crate) fn refresh(&mut self) {
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
        self.next = None;
        self.listed = false;
        self.refresh();
    }

    pub(crate) fn defer_search(&mut self, words: &str) -> u64 {
        self.search(words);
        self.deferred_search = Some(self.list_revision);
        self.list_revision
    }

    pub(crate) fn finish_search(&mut self, revision: u64) {
        if self.deferred_search == Some(revision) {
            self.deferred_search = None;
        }
    }

    pub(crate) fn select(&mut self, thread: Option<ThreadId>) {
        self.open = thread;
        self.events.clear();
        self.before = None;
        self.loaded = false;
        self.event_revision = self.event_revision.wrapping_add(1);
        self.event_needed = thread.map(|_| Read::First);
    }

    /// `thread` was hidden or removed: its row, its open messages and every read in flight
    /// about it go, so no reply sent before the brain hid it lands.
    pub(crate) fn deleted(&mut self, thread: ThreadId) {
        self.rows.retain(|row| row.id != thread);
        self.list_revision = self.list_revision.wrapping_add(1);
        if self.list_pending.is_some() && self.list_needed.is_none() {
            self.list_needed = Some(Read::Repair);
        }
        if self.open == Some(thread) {
            self.select(None);
        }
    }

    pub(crate) fn more_threads(&mut self) {
        if self.next.is_some() && !self.listing() {
            self.list_needed = Some(Read::More);
        }
    }

    pub(crate) fn older_events(&mut self) {
        if self.before.is_some() && !self.reading() {
            self.event_needed = Some(Read::More);
        }
    }

    pub(crate) fn run_state(&mut self, thread: ThreadId) {
        // shortcut: Stop and failure are told before their outcome is written, so this read can
        // miss it until the next RunState; refresh on the store's ack if that shows.
        if self.open == Some(thread) && self.event_needed != Some(Read::First) {
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

    /// Marks requests in flight before handing them to the nonblocking sender. A failed send
    /// disconnects this state; changing a query or selection never frees an unanswered slot.
    pub(crate) fn requests(&mut self) -> Vec<DataToBrain> {
        if !self.connected || !self.shown {
            return Vec::new();
        }
        let mut requests = Vec::new();
        if self.list_pending.is_none()
            && self.deferred_search.is_none()
            && let Some(read) = self.list_needed.take()
        {
            let request = self.number();
            self.list_pending = Some(Pending {
                request,
                revision: self.list_revision,
                read,
                stale: false,
            });
            requests.push(DataToBrain::PageRequest {
                request,
                page: PageOf::Threads {
                    search: self.search.clone(),
                    after: if read == Read::More { self.next } else { None },
                },
            });
        }
        if self.event_pending.is_none()
            && let Some(thread) = self.open
            && let Some(read) = self.event_needed.take()
        {
            let request = self.number();
            self.event_pending = Some(Pending {
                request,
                revision: self.event_revision,
                read,
                stale: false,
            });
            requests.push(DataToBrain::PageRequest {
                request,
                page: PageOf::Events {
                    thread,
                    before: if read == Read::More {
                        self.before
                    } else {
                        None
                    },
                },
            });
        }
        requests
    }

    /// Applies a page frame and returns its credit: the whole frame, used or dropped, since the
    /// brain counted all of it against the window.
    pub(crate) fn receive(&mut self, message: DataToShell, bytes: u32) -> u32 {
        self.apply(message);
        bytes
    }

    pub(crate) fn apply(&mut self, message: DataToShell) {
        match message {
            DataToShell::ThreadList {
                request,
                threads,
                next,
            } => {
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
                match pending.read {
                    Read::First => {
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
                }
                self.listed = true;
                self.failed = false;
            }
            DataToShell::Page {
                request,
                thread,
                events,
                before,
            } => {
                let Some(pending) = self
                    .event_pending
                    .filter(|pending| pending.request == request)
                else {
                    return;
                };
                self.event_pending = None;
                if pending.revision != self.event_revision || self.open != Some(thread) {
                    return;
                }
                if pending.read == Read::First || !self.loaded {
                    self.events = events;
                    self.before = before;
                } else if pending.read == Read::Repair
                    && before.is_some()
                    && self
                        .events
                        .first()
                        .is_some_and(|shown| events.iter().all(|event| event.id > shown.id))
                {
                    // No overlap with what's shown: the gap is only reachable through this page's cursor.
                    self.events = events;
                    self.before = before;
                } else if pending.read == Read::Repair {
                    let newest = self.events.first().map(|event| event.id);
                    let fresh = events
                        .into_iter()
                        .take_while(|event| newest.is_none_or(|newest| event.id > newest));
                    self.events.splice(0..0, fresh);
                } else {
                    for event in events {
                        if !self.events.iter().any(|old| old.id == event.id) {
                            self.events.push(event);
                        }
                    }
                    self.before = before;
                }
                self.loaded = true;
                self.failed = false;
            }
            DataToShell::PageFailed { request } => self.read_failed(request),
            DataToShell::AssistantText { .. }
            | DataToShell::StepCommit { .. }
            | DataToShell::RunEnd { .. }
            | DataToShell::SceneSnapshot { .. }
            | DataToShell::ScenesRemoved { .. } => {}
        }
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
            pending.revision == self.event_revision
        } else {
            return;
        };
        self.failed |= current;
    }

    fn number(&mut self) -> u32 {
        self.counter = self.counter.wrapping_add(1);
        self.counter
    }
}
