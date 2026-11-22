use crate::link::{LinkHandle, Session};
use crate::threads::Threads;
use kumochi_protocol::message::{ControlToBrain, DraftId, IssueOp};
use std::cell::Cell;
use std::rc::Rc;

pub(in crate::presence) struct Route<T> {
    session: Session,
    request: u64,
    draft: DraftId,
    target: T,
    live: Rc<Cell<bool>>,
    delivered: bool,
}

/// What a review decision has the shell do, applied at the point it arises.
pub(super) enum Effect<T> {
    Threads(Update),
    /// After this turn, retire the view's pending consent for this review.
    RetireView(T, u64),
}

pub(super) enum Update {
    Submitted(DraftId, Option<u64>),
    Started(DraftId, u64),
    Cancelled(u64, DraftId),
}

impl Update {
    pub(super) fn apply(self, model: &mut Threads) {
        match self {
            Self::Submitted(draft, request) => model.issue_submitted(draft, request),
            Self::Started(draft, request) => model.issue_review_started(draft, request),
            Self::Cancelled(request, draft) => model.cancel_issue_review(request, draft),
        }
    }
}

/// The owner's review state, borrowed for one decision. Sends go straight to the link, and each
/// effect is applied where it arises, so sends and effects keep their order.
pub(super) struct Reviews<'a, T> {
    pub(super) route: &'a mut Option<Route<T>>,
    pub(super) request: &'a mut u64,
    pub(super) session: Option<&'a Session>,
    pub(super) link: &'a LinkHandle,
    pub(super) apply: &'a mut dyn FnMut(Effect<T>),
}

impl<T> Reviews<'_, T> {
    fn message(&mut self, session: &Session, op: IssueOp) -> Option<u64> {
        let request = self.request.checked_add(1)?;
        *self.request = request;
        self.link
            .send_control(
                session,
                ControlToBrain::Issue {
                    session: session.issue_session(),
                    request,
                    op: Box::new(op),
                },
            )
            .then_some(request)
    }

    pub(super) fn begin(&mut self, op: IssueOp, target: T, live: Rc<Cell<bool>>) -> Option<u64> {
        let IssueOp::Rearm { draft, .. } = op else {
            return None;
        };
        self.retire();
        let session = self.session?.clone();
        let Some(request) = self.message(&session, op) else {
            (self.apply)(Effect::Threads(Update::Submitted(draft, None)));
            self.link.end(&session);
            return None;
        };
        *self.route = Some(Route {
            session,
            request,
            draft,
            target,
            live,
            delivered: false,
        });
        (self.apply)(Effect::Threads(Update::Started(draft, request)));
        Some(request)
    }

    fn cancel(&mut self, route: Route<T>) {
        route.live.set(false);
        if self.session.is_some_and(|session| {
            session.generation() == route.session.generation()
                && session.issue_session() == route.session.issue_session()
        }) && self
            .message(
                &route.session,
                IssueOp::CancelReview {
                    draft: route.draft,
                    review: route.request,
                },
            )
            .is_none()
        {
            self.link.end(&route.session);
        }
        (self.apply)(Effect::Threads(Update::Cancelled(
            route.request,
            route.draft,
        )));
        (self.apply)(Effect::RetireView(route.target, route.request));
    }

    pub(super) fn retire(&mut self) {
        if let Some(route) = self.route.take() {
            self.cancel(route);
        }
    }

    pub(super) fn cancel_review(&mut self, review: u64) {
        if self
            .route
            .as_ref()
            .is_some_and(|route| route.request == review)
        {
            self.retire();
        }
    }

    pub(super) fn fail(&mut self, request: u64, draft: DraftId) {
        if self
            .route
            .as_ref()
            .is_some_and(|route| route.request == request && route.draft == draft)
        {
            self.retire();
        }
    }

    pub(super) fn confirm(&mut self, review: u64, op: IssueOp) -> bool {
        if self
            .route
            .as_ref()
            .is_none_or(|route| route.request != review || !route.delivered)
        {
            return false;
        }
        let route = self
            .route
            .take()
            .expect("the current offered route was checked");
        route.live.set(false);
        let queued = self.send(op);
        if !queued {
            self.cancel(route);
        }
        queued
    }

    pub(super) fn accepts(&self, request: u64) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| route.request == request && route.live.get() && route.delivered)
    }

    /// Marks the one delivery of a live review's card: where it goes and what decides it there.
    pub(super) fn deliver(
        &mut self,
        request: u64,
        draft: DraftId,
    ) -> Option<(T, Session, Rc<Cell<bool>>)>
    where
        T: Clone,
    {
        let route = self.route.as_mut().filter(|route| {
            route.request == request && route.draft == draft && route.live.get() && !route.delivered
        })?;
        route.delivered = true;
        Some((
            route.target.clone(),
            route.session.clone(),
            Rc::clone(&route.live),
        ))
    }

    pub(super) fn send(&mut self, op: IssueOp) -> bool {
        let draft = match op {
            IssueOp::Consent { draft, .. }
            | IssueOp::Discard { draft }
            | IssueOp::CheckAgain { draft } => draft,
            IssueOp::Rearm { .. } | IssueOp::CancelReview { .. } => return false,
        };
        self.retire();
        let request = self
            .session
            .cloned()
            .and_then(|session| self.message(&session, op));
        (self.apply)(Effect::Threads(Update::Submitted(draft, request)));
        request.is_some()
    }
}
