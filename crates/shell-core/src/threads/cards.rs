//! The two history views share only displayed ids, authoritative watermarks and full cards.

use std::collections::HashMap;

use kumochi_protocol::message::{
    ActionState, ApprovalRev, DraftId, IssueCard, IssueRefused, IssueUnavailableKind,
    MAX_ISSUE_CARDS, StateVersion, ThreadId,
};

use super::View;

pub(super) struct Watermark {
    pub(super) version: StateVersion,
    pub(super) state: ActionState,
    pub(super) approval: Option<ApprovalRev>,
}

pub struct Entry {
    thread: ThreadId,
    views: [bool; 2],
    pub(super) mark: Option<Watermark>,
    pub(super) full: Option<IssueCard>,
    needed: bool,
    flight: Option<u32>,
    attempt: Option<Attempt>,
    pub feedback: Option<Feedback>,
}

struct Attempt {
    request: u64,
    pending: bool,
    review: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    QueueFailed,
    Unavailable(IssueUnavailableKind),
    Refused(IssueRefused),
}

impl Entry {
    pub fn review_blocked(&self) -> bool {
        matches!(
            self.feedback,
            Some(Feedback::Unavailable(IssueUnavailableKind::AttemptNotSaved))
        )
    }

    pub fn pending(&self) -> bool {
        self.attempt.as_ref().is_some_and(|attempt| attempt.pending)
    }

    pub fn pending_action(&self) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|attempt| attempt.pending && !attempt.review)
    }

    pub fn pending_review(&self) -> bool {
        self.attempt
            .as_ref()
            .is_some_and(|attempt| attempt.pending && attempt.review)
    }

    pub fn pending_review_request(&self) -> Option<u64> {
        self.attempt
            .as_ref()
            .filter(|attempt| attempt.pending && attempt.review)
            .map(|attempt| attempt.request)
    }

    pub fn state(&self) -> Option<&ActionState> {
        self.mark.as_ref().map(|mark| &mark.state)
    }

    pub fn card(&self) -> Option<&IssueCard> {
        self.full.as_ref()
    }

    pub fn loading(&self) -> bool {
        self.needed || self.flight.is_some()
    }
}

struct Batch {
    request: u32,
    ids: Vec<DraftId>,
}

#[derive(Default)]
pub(super) struct Cards {
    displayed: [(Option<ThreadId>, Vec<DraftId>); 2],
    pub(super) entries: HashMap<DraftId, Entry>,
    pending: Vec<Batch>,
    refresh: [bool; 2],
    pub(super) broken: bool,
}

impl std::fmt::Debug for Cards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cards")
            .field("displayed", &self.entries.len())
            .field("pending", &self.pending.len())
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

fn index(view: View) -> usize {
    match view {
        View::Browse => 0,
        View::Chat => 1,
    }
}

impl Cards {
    pub(super) fn entry(&self, view: View, draft: DraftId) -> Option<&Entry> {
        self.entries
            .get(&draft)
            .filter(|entry| entry.views[index(view)])
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn shown(&mut self, view: View, thread: Option<ThreadId>, ids: Vec<DraftId>) {
        let at = index(view);
        self.displayed[at] = (thread, ids);
        for entry in self.entries.values_mut() {
            entry.views[at] = false;
        }
        if !self.broken
            && let Some(thread) = thread
        {
            for &id in &self.displayed[at].1 {
                let entry = self.entries.entry(id).or_insert_with(|| Entry {
                    thread,
                    views: [false; 2],
                    mark: None,
                    full: None,
                    needed: true,
                    flight: None,
                    attempt: None,
                    feedback: None,
                });
                if entry.thread != thread {
                    self.broken = true;
                    break;
                }
                entry.views[at] = true;
            }
        }
        self.entries
            .retain(|_, entry| entry.views[0] || entry.views[1]);
    }

    pub(super) fn wanted(&self, view: View) -> Vec<DraftId> {
        self.displayed[index(view)]
            .1
            .iter()
            .copied()
            .filter(|id| {
                self.entries
                    .get(id)
                    .is_some_and(|entry| entry.needed && entry.flight.is_none())
                    && !self.pending.iter().any(|batch| batch.ids.contains(id))
            })
            .take(MAX_ISSUE_CARDS)
            .collect()
    }

    pub(super) fn start(&mut self, request: u32, ids: &[DraftId]) {
        let mut unique = Vec::new();
        for &id in ids {
            if unique.contains(&id) {
                continue;
            }
            unique.push(id);
            if let Some(entry) = self.entries.get_mut(&id)
                && entry.flight.is_none()
            {
                entry.needed = false;
                entry.flight = Some(request);
            }
        }
        self.pending.push(Batch {
            request,
            ids: unique,
        });
    }

    pub(super) fn pending(&self, request: u32) -> bool {
        self.pending.iter().any(|batch| batch.request == request)
    }

    pub(super) fn done(&mut self, request: u32) {
        let Some(at) = self
            .pending
            .iter()
            .position(|batch| batch.request == request)
        else {
            return;
        };
        for id in self.pending.remove(at).ids {
            if let Some(entry) = self.entries.get_mut(&id)
                && entry.flight == Some(request)
            {
                entry.flight = None;
            }
        }
    }

    pub(super) fn full(&mut self, request: u32, mut card: IssueCard) {
        if self.broken {
            return;
        }
        let Some(entry) = self.entries.get_mut(&card.draft) else {
            return;
        };
        if entry.flight != Some(request) || entry.thread != card.thread {
            return;
        }
        if card.version.0 == 0 {
            self.broken = true;
            return;
        }
        if let Some(mark) = &entry.mark {
            if card.version < mark.version {
                return;
            }
            if card.version == mark.version {
                if card.state != mark.state {
                    self.broken = true;
                    return;
                }
                if entry.full.is_some() {
                    return;
                }
                card.state = mark.state.clone();
                card.approval = mark.approval;
            }
        }
        entry.mark = Some(Watermark {
            version: card.version,
            state: card.state.clone(),
            approval: card.approval,
        });
        entry.full = Some(card);
        entry.needed = false;
    }

    pub(super) fn state(
        &mut self,
        draft: DraftId,
        version: StateVersion,
        state: ActionState,
        approval: Option<ApprovalRev>,
        request: Option<u64>,
    ) {
        if self.broken {
            return;
        }
        if draft.0 == 0 || version.0 == 0 {
            self.broken = true;
            return;
        }
        let Some(entry) = self.entries.get_mut(&draft) else {
            for (at, (thread, _)) in self.displayed.iter().enumerate() {
                if thread.is_some() {
                    self.refresh[at] = true;
                }
            }
            return;
        };
        if let Some(mark) = &entry.mark {
            if version < mark.version {
                return;
            }
            if version == mark.version {
                if state != mark.state || approval != mark.approval {
                    self.broken = true;
                } else if let Some(attempt) = &mut entry.attempt
                    && Some(attempt.request) == request
                {
                    attempt.pending = false;
                }
                return;
            }
        }
        if let Some(attempt) = &mut entry.attempt
            && Some(attempt.request) == request
        {
            attempt.pending = false;
        }
        entry.feedback = None;
        entry.mark = Some(Watermark {
            version,
            state,
            approval,
        });
        entry.full = None;
        entry.needed = true;
    }

    pub(super) fn submitted(&mut self, draft: DraftId, request: Option<u64>) {
        if let Some(entry) = self.entries.get_mut(&draft) {
            entry.attempt = request.map(|request| Attempt {
                request,
                pending: true,
                review: false,
            });
            if !entry.review_blocked() {
                entry.feedback = request.is_none().then_some(Feedback::QueueFailed);
            }
        }
    }

    pub(super) fn unavailable(&mut self, request: u64, draft: DraftId, kind: IssueUnavailableKind) {
        let Some(entry) = self.entries.get_mut(&draft) else {
            return;
        };
        let Some(attempt) = &mut entry.attempt else {
            return;
        };
        if !attempt.pending || attempt.request != request {
            return;
        }
        attempt.pending = false;
        if !entry.review_blocked() {
            entry.feedback = Some(Feedback::Unavailable(kind));
        }
    }

    pub(super) fn review_started(&mut self, draft: DraftId, request: u64) {
        if let Some(entry) = self.entries.get_mut(&draft) {
            if entry.review_blocked() {
                return;
            }
            entry.attempt = Some(Attempt {
                request,
                pending: true,
                review: true,
            });
            entry.feedback = None;
        }
    }

    pub(super) fn cancel_review(&mut self, request: u64, draft: DraftId) {
        if let Some(entry) = self.entries.get_mut(&draft)
            && entry
                .attempt
                .as_ref()
                .is_some_and(|attempt| attempt.request == request && attempt.review)
        {
            entry.attempt = None;
        }
    }

    pub(super) fn review(&mut self, request: u64, card: IssueCard) -> bool {
        if self.broken || card.version.0 == 0 {
            return false;
        }
        let Some(entry) = self.entries.get_mut(&card.draft) else {
            return false;
        };
        if entry.thread != card.thread
            || entry.review_blocked()
            || entry
                .attempt
                .as_ref()
                .is_none_or(|attempt| attempt.request != request || !attempt.review)
            || entry
                .mark
                .as_ref()
                .is_some_and(|mark| card.version < mark.version)
        {
            return false;
        }
        if entry.mark.as_ref().is_some_and(|mark| {
            card.version == mark.version
                && (card.state != mark.state || card.approval != mark.approval)
        }) || entry
            .full
            .as_ref()
            .is_some_and(|full| full.version == card.version && *full != card)
        {
            self.broken = true;
            return false;
        }
        entry.mark = Some(Watermark {
            version: card.version,
            state: card.state.clone(),
            approval: card.approval,
        });
        entry.full = Some(card);
        entry.needed = false;
        entry.flight = None;
        if let Some(attempt) = &mut entry.attempt {
            attempt.pending = false;
        }
        entry.feedback = None;
        true
    }

    pub(super) fn refused(
        &mut self,
        request: u64,
        draft: DraftId,
        version: StateVersion,
        approval: Option<ApprovalRev>,
        reason: IssueRefused,
    ) {
        let Some(entry) = self.entries.get_mut(&draft) else {
            return;
        };
        if entry
            .attempt
            .as_ref()
            .is_none_or(|attempt| attempt.request != request)
            || entry
                .mark
                .as_ref()
                .is_none_or(|mark| mark.version != version || mark.approval != approval)
        {
            return;
        }
        if let Some(attempt) = &mut entry.attempt {
            attempt.pending = false;
        }
        if !entry.review_blocked() {
            entry.feedback = Some(Feedback::Refused(reason));
        }
    }

    pub(super) fn take_refresh(&mut self, view: View) -> bool {
        std::mem::take(&mut self.refresh[index(view)])
    }
}
