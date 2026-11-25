//! One reserved filing/recovery job; store acknowledgements own every durable phase.

use std::{fmt, sync::Arc};

use kumochi_protocol::{
    Secret,
    message::{
        ControlToShell, Decision, DraftId, FailReason, Incarnation, IssueOp, IssueRefused,
        IssueSession, IssueUnavailableKind,
    },
};
use tokio::{task::JoinHandle, time::Instant};

use super::{Brain, Effect, StoreOp};
use crate::connections::{
    github::{
        GitHubToken,
        device::Stamp,
        issues::{CreateOutcome, IssueContent, create_issue},
    },
    http_client::{HttpError, SharedHttp},
};
use crate::issues::{Action, Answer, Lookup, Stage, Write};

mod network;
mod reviews;
pub(in crate::runtime) mod transport;
use network::{Failure, Prepared};
use reviews::{Offered, Rearm};
use transport::{BoundedHttp, Control};

#[cfg(test)]
#[path = "issues/owner_tests.rs"]
mod owner_tests;

#[derive(Debug, Default)]
pub(super) struct IssueEpochs(u64);

impl IssueEpochs {
    fn reserve(&mut self, incarnation: Incarnation) -> IssueSession {
        self.0 = self
            .0
            .checked_add(1)
            .expect("issue session identities never wrap");
        IssueSession {
            incarnation,
            epoch: self.0,
        }
    }
}

#[derive(Default)]
pub(super) struct Filing {
    next: u64,
    pub(super) session: Option<IssueSession>,
    last_request: Option<(IssueSession, u64)>,
    offers: Vec<Offered>,
    rearms: Vec<Rearm>,
    job: Option<Job>,
    replies: Vec<Reply>,
    #[cfg(debug_assertions)]
    live_block: FilingBlock,
}

struct Reply {
    serial: u64,
    epoch: u64,
    request: u64,
    draft: DraftId,
    forced: Option<IssueRefused>,
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FilingBlock {
    #[default]
    Off,
    Armed,
    Attempted,
}

#[cfg(debug_assertions)]
impl FilingBlock {
    fn from_flag(flag: Option<&std::ffi::OsStr>) -> Self {
        if flag.is_some() {
            Self::Armed
        } else {
            Self::Off
        }
    }
}

impl fmt::Debug for Filing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Filing")
            .field("busy", &self.job.is_some())
            .field("replies", &self.replies.len())
            .field("offers", &self.offers.len())
            .field("rearms", &self.rearms.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum AfterWrite {
    Admission,
    Read,
    Posting,
    Result,
    Cleanup,
}

enum Phase {
    Store { serial: u64, after: AfterWrite },
    Construction(JoinHandle<Result<SharedHttp, HttpError>>),
    Network(JoinHandle<Outcome>),
    Completed,
}

struct Job {
    id: u64,
    epoch: u64,
    request: u64,
    draft: DraftId,
    stamp: Stamp,
    check: bool,
    control: Arc<Control>,
    admission: Option<Write>,
    action: Option<Action>,
    token: Option<Secret<GitHubToken>>,
    prepared: Option<Prepared>,
    phase: Phase,
}

enum Outcome {
    Built(Result<SharedHttp, HttpError>),
    Preflight(Result<Prepared, Failure>),
    Checked(Result<Lookup, Failure>),
    Posted(CreateOutcome),
    Stopped,
    Deadline,
    ReviewDeadline,
}

pub(super) struct Event {
    id: u64,
    outcome: Outcome,
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IssueJobEvent")
    }
}

impl Filing {
    pub(super) fn new() -> Self {
        Self {
            #[cfg(debug_assertions)]
            live_block: FilingBlock::from_flag(
                std::env::var_os(kumochi_protocol::constants::LIVE_NO_FILING_ENV).as_deref(),
            ),
            ..Self::default()
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn report_live_block(&self) {
        if self.live_block != FilingBlock::Off
            && let Some(session) = self.session
        {
            tracing::info!(
                build = kumochi_protocol::BUILD_ID,
                pid = std::process::id(),
                incarnation = session.incarnation.0,
                issue_epoch = session.epoch,
                attempted = self.live_block == FilingBlock::Attempted,
                "live filing block armed"
            );
        }
    }

    #[cfg(debug_assertions)]
    fn note_filing_attempt(&mut self) {
        if self.live_block == FilingBlock::Armed {
            self.live_block = FilingBlock::Attempted;
            if let Some(session) = self.session {
                tracing::warn!(
                    build = kumochi_protocol::BUILD_ID,
                    pid = std::process::id(),
                    incarnation = session.incarnation.0,
                    issue_epoch = session.epoch,
                    "live filing attempt blocked"
                );
            }
        }
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn set_live_block_for_test(&mut self, flag: Option<&std::ffi::OsStr>) {
        self.live_block = FilingBlock::from_flag(flag);
    }

    fn serial(&mut self) -> u64 {
        self.next = self
            .next
            .checked_add(1)
            .expect("issue operation identities never wrap");
        self.next
    }

    pub(super) fn cancel(&mut self) {
        if let Some(job) = &self.job {
            job.control.cancel();
        }
        self.cancel_reviews();
    }

    pub(super) fn pending(&self) -> bool {
        self.job.is_some() || !self.replies.is_empty() || !self.rearms.is_empty()
    }

    #[cfg(test)]
    pub(super) fn deadline_for_test(&self) -> Option<Instant> {
        self.job.as_ref().map(|job| job.control.deadline())
    }

    #[cfg(test)]
    pub(super) fn review_counts(&self) -> (usize, usize) {
        (self.offers.len(), self.rearms.len())
    }

    pub(super) async fn next(&mut self) -> Event {
        let review = self
            .rearms
            .iter()
            .filter(|rearm| rearm.live)
            .min_by_key(|rearm| rearm.deadline)
            .map(|rearm| (rearm.serial, rearm.deadline));
        let deadline = self
            .job
            .as_ref()
            .filter(|job| !job.control.cancelled())
            .map(|job| job.control.deadline());
        let id = self.job.as_ref().map_or(0, |job| job.id);
        let job = &mut self.job;
        tokio::select! {
            biased;
            () = super::sleep_until(review.map(|(_, deadline)| deadline)) => Event {
                id: review.expect("only a pending review has a deadline").0,
                outcome: Outcome::ReviewDeadline,
            },
            () = super::sleep_until(deadline) => Event { id, outcome: Outcome::Deadline },
            outcome = async {
                let Some(job) = job else {
                    return std::future::pending().await;
                };
                let outcome = match &mut job.phase {
                    Phase::Construction(task) => Outcome::Built(task.await.unwrap_or(Err(HttpError::Configuration))),
                    Phase::Network(task) => task.await.unwrap_or(Outcome::Stopped),
                    Phase::Store { .. } | Phase::Completed => return std::future::pending().await,
                };
                job.phase = Phase::Completed;
                outcome
            } => Event { id, outcome },
        }
    }

    fn write(&mut self, job: &mut Job, after: AfterWrite, write: Write) -> Effect {
        let serial = self.serial();
        job.phase = Phase::Store { serial, after };
        let scope = matches!(
            after,
            AfterWrite::Admission | AfterWrite::Read | AfterWrite::Posting
        )
        .then(|| job.control.scope().clone());
        Effect::Store(StoreOp::Issue {
            serial,
            write,
            scope,
        })
    }
}

impl Brain {
    pub(crate) fn prepare_issue_session(&mut self, incarnation: Incarnation) -> IssueSession {
        let session = self.reserve_issue_session(incarnation);
        self.install_issue_session(session);
        session
    }

    pub(crate) fn install_issue_session(&mut self, session: IssueSession) {
        self.filing.cancel_reviews();
        self.filing.session = Some(session);
    }

    pub(crate) fn reserve_issue_session(&mut self, incarnation: Incarnation) -> IssueSession {
        self.issue_epochs.reserve(incarnation)
    }

    pub(crate) fn issue_session(&self) -> Option<IssueSession> {
        self.filing.session
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one synchronous ingress removes permission before any non-transport refusal"
    )]
    pub(super) fn issue_op(
        &mut self,
        session: IssueSession,
        request: u64,
        op: &IssueOp,
        epoch: Option<u64>,
    ) -> Option<Vec<Effect>> {
        #[cfg(debug_assertions)]
        if matches!(
            op,
            IssueOp::Consent {
                decision: Decision::File | Decision::FileAnyway,
                ..
            } | IssueOp::CheckAgain { .. }
        ) {
            self.filing.note_filing_attempt();
        }
        let draft = match op {
            IssueOp::Consent { draft, .. }
            | IssueOp::CheckAgain { draft }
            | IssueOp::Discard { draft }
            | IssueOp::Rearm { draft, .. }
            | IssueOp::CancelReview { draft, .. } => *draft,
        };
        if self.issue_session() != Some(session) {
            return None;
        }
        let offered = if let IssueOp::Consent { approval, .. } = op {
            self.filing
                .offers
                .iter()
                .position(|offer| offer.approval == *approval)
                .map(|index| self.filing.offers.swap_remove(index))
        } else {
            None
        };
        if let IssueOp::CancelReview { review, .. } = op {
            self.filing.cancel_review(session, *review, draft);
        }
        if let Some((previous, last)) = self.filing.last_request
            && previous == session
            && request <= last
        {
            return Some(self.issue_unavailable(self.epoch, request, draft));
        }
        self.filing.last_request = Some((session, request));
        if self.filing.replies.len() >= super::CONTROL_QUEUE {
            return None;
        }
        match op {
            IssueOp::CancelReview { .. } => return Some(Vec::new()),
            IssueOp::Rearm {
                draft,
                version,
                hash,
            } => {
                return Some(self.issue_rearm(session, request, *draft, *version, *hash, epoch));
            }
            _ => {}
        }
        let available = epoch == Some(self.epoch) && self.filing.job.is_none();
        #[cfg(debug_assertions)]
        let available = available && self.filing.live_block == FilingBlock::Off;
        let (write, start, forced) = match op {
            IssueOp::Consent {
                draft,
                hash,
                approval: _,
                decision,
            } => {
                let Some(offered) = offered else {
                    return Some(self.issue_unavailable(self.epoch, request, *draft));
                };
                if offered.session != session
                    || offered.draft != *draft
                    || offered.hash != *hash
                    || *decision != Decision::File
                {
                    return Some(self.issue_unavailable(self.epoch, request, *draft));
                }
                let start = available;
                let mut live = self.account.github.issue_snapshot();
                live.capacity = start;
                (
                    Write::Consent {
                        draft: *draft,
                        at: offered.version,
                        hash: offered.hash,
                        approval: offered.approval,
                        decision: *decision,
                        live,
                    },
                    start,
                    None,
                )
            }
            IssueOp::CheckAgain { draft } => (
                Write::Read(*draft),
                available,
                (!available).then_some(IssueRefused::NotAllowedNow),
            ),
            IssueOp::Discard { draft } => (Write::Discard(*draft), false, None),
            IssueOp::Rearm { .. } | IssueOp::CancelReview { .. } => {
                unreachable!("review operations are routed before filing")
            }
        };
        if !start {
            let serial = self.filing.serial();
            self.filing.replies.push(Reply {
                serial,
                epoch: self.epoch,
                request,
                draft,
                forced,
            });
            return Some(vec![Effect::Store(StoreOp::Issue {
                serial,
                write,
                scope: None,
            })]);
        }
        let check = matches!(write, Write::Read(_));
        let control = Control::new(self.account.github.tuning);
        let stamp = self.account.github.watch_issue(&control);
        let mut job = Job {
            id: self.filing.serial(),
            epoch: self.epoch,
            request,
            draft,
            stamp,
            check,
            control,
            admission: (!check).then(|| write.clone()),
            action: None,
            token: None,
            prepared: None,
            phase: Phase::Completed,
        };
        let after = if check {
            AfterWrite::Read
        } else {
            AfterWrite::Admission
        };
        let effect = self.filing.write(&mut job, after, write);
        self.filing.job = Some(job);
        Some(vec![effect])
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one acknowledgement router retains entering-phase ownership and compensation"
    )]
    pub(super) fn issue_written(&mut self, serial: u64, reply: Result<Answer, ()>) -> Vec<Effect> {
        if let Some(index) = self
            .filing
            .rearms
            .iter()
            .position(|rearm| rearm.serial == serial)
        {
            let rearm = self.filing.rearms.swap_remove(index);
            return self.issue_review_written(&rearm, reply);
        }
        if let Some(index) = self
            .filing
            .replies
            .iter()
            .position(|pending| pending.serial == serial)
        {
            let pending = self.filing.replies.remove(index);
            return reply.map_or_else(
                |()| self.issue_unavailable(pending.epoch, pending.request, pending.draft),
                |answer| {
                    self.issue_notice(
                        pending.epoch,
                        Some(pending.request),
                        pending.draft,
                        &answer,
                        pending.forced,
                    )
                },
            );
        }
        let Some(mut job) = self.filing.job.take() else {
            return Vec::new();
        };
        let Phase::Store {
            serial: waiting,
            after,
        } = job.phase
        else {
            self.filing.job = Some(job);
            return Vec::new();
        };
        if waiting != serial {
            self.filing.job = Some(job);
            return Vec::new();
        }
        let Ok(answer) = reply else {
            if matches!(after, AfterWrite::Admission)
                && let Some(Write::Consent {
                    draft,
                    at,
                    hash,
                    approval,
                    live,
                    ..
                }) = job.admission.take()
            {
                let write = Write::CancelledConsent {
                    draft,
                    at,
                    hash,
                    approval,
                    live,
                };
                let effect = self.filing.write(&mut job, AfterWrite::Cleanup, write);
                self.filing.job = Some(job);
                return vec![effect];
            }
            if matches!(after, AfterWrite::Result | AfterWrite::Posting) {
                let effect = self.issue_interrupted(&mut job);
                self.filing.job = Some(job);
                return vec![effect];
            }
            return self.issue_unavailable_kind(
                job.epoch,
                job.request,
                job.draft,
                if job.control.may_have_sent() {
                    IssueUnavailableKind::AttemptNotSaved
                } else {
                    IssueUnavailableKind::Action
                },
            );
        };
        job.admission = None;
        if matches!(answer, Answer::Gone) {
            return self.issue_unavailable_kind(
                job.epoch,
                job.request,
                job.draft,
                if job.control.may_have_sent() {
                    IssueUnavailableKind::AttemptNotSaved
                } else {
                    IssueUnavailableKind::Action
                },
            );
        }
        let active = match (&answer, after) {
            (Answer::Changed(action), AfterWrite::Admission) => {
                matches!(action.stage, Stage::Preflight { .. })
            }
            (Answer::Unchanged(action), AfterWrite::Read) => {
                matches!(action.stage, Stage::Unknown { .. })
            }
            (Answer::Changed(action), AfterWrite::Posting) => {
                matches!(action.stage, Stage::Posting { .. })
            }
            _ => false,
        };
        let forced =
            (!active && matches!(after, AfterWrite::Read)).then_some(IssueRefused::NotAllowedNow);
        let mut effects = self.issue_notice(
            job.epoch,
            (!active).then_some(job.request),
            job.draft,
            &answer,
            forced,
        );
        if !active {
            return effects;
        }
        let action = match answer {
            Answer::Changed(action) | Answer::Unchanged(action) => action,
            Answer::Refused(_, _) | Answer::Gone => unreachable!("an active phase has an action"),
        };
        job.action = Some(action);
        if stopped(&job) {
            effects.push(self.issue_interrupted(&mut job));
        } else if matches!(after, AfterWrite::Posting) {
            effects.extend(self.issue_send(&mut job));
        } else if let Some(token) = self.account.github.issue_token(
            job.stamp,
            job.action.as_ref().expect("the acknowledged action"),
        ) {
            job.token = Some(token);
            let factory = Arc::clone(&self.account.net.http);
            job.phase = Phase::Construction(tokio::task::spawn_blocking(move || factory()));
        } else if job.check {
            effects.extend(self.issue_notice(
                job.epoch,
                Some(job.request),
                job.draft,
                &Answer::Unchanged(job.action.take().expect("the acknowledged action")),
                Some(IssueRefused::NotConnected),
            ));
            return effects;
        } else {
            effects.push(self.issue_unsent(&mut job, FailReason::NotConnected));
        }
        self.filing.job = Some(job);
        effects
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one event router retains the entering job phase through cancellation and settlement"
    )]
    pub(super) fn issue_event(&mut self, event: Event) -> Vec<Effect> {
        if matches!(event.outcome, Outcome::ReviewDeadline) {
            return self.issue_review_expired(event.id);
        }
        let Some(mut job) = self.filing.job.take() else {
            return Vec::new();
        };
        if job.id != event.id {
            self.filing.job = Some(job);
            return Vec::new();
        }
        if matches!(event.outcome, Outcome::Deadline) {
            job.control.cancel();
            self.filing.job = Some(job);
            return Vec::new();
        }
        let ended = stopped(&job);
        let unauthorized = match &event.outcome {
            Outcome::Preflight(Err(failure)) | Outcome::Checked(Err(failure)) => {
                failure.unauthorized
            }
            Outcome::Posted(CreateOutcome::Definite {
                status: Some(401), ..
            }) => true,
            _ => false,
        };
        let mut effects = if unauthorized {
            self.account
                .issue_unauthorized(job.stamp)
                .into_iter()
                .map(Effect::Control)
                .collect()
        } else {
            Vec::new()
        };
        if ended {
            effects.push(self.issue_interrupted(&mut job));
        } else {
            match event.outcome {
                Outcome::Built(Ok(http)) => {
                    let http = BoundedHttp::new(http, Arc::clone(&job.control));
                    let control = Arc::clone(&job.control);
                    let token = job
                        .token
                        .take()
                        .expect("construction retains its read credential");
                    let action = job
                        .action
                        .clone()
                        .expect("construction follows a durable admission");
                    let check = job.check;
                    job.phase = Phase::Network(tokio::spawn(async move {
                        if check {
                            Outcome::Checked(network::check(&http, &token, &action).await)
                        } else {
                            Outcome::Preflight(
                                network::preflight(http, token, &action, &control).await,
                            )
                        }
                    }));
                }
                Outcome::Preflight(Ok(prepared)) => {
                    let action = job.action.as_ref().expect("preflight has an action");
                    let write = Write::Preflighted {
                        draft: action.draft,
                        at: action.version,
                        result: Ok(prepared.mark.clone()),
                    };
                    job.prepared = Some(prepared);
                    effects.push(self.filing.write(&mut job, AfterWrite::Posting, write));
                }
                Outcome::Preflight(Err(failure)) | Outcome::Checked(Err(failure)) => {
                    effects.push(self.issue_unsent(&mut job, failure.reason));
                }
                Outcome::Checked(Ok(lookup)) => {
                    let action = job.action.as_ref().expect("a lookup has an action");
                    let write = Write::LookedUp {
                        draft: action.draft,
                        episode: action.uncertain,
                        lookup,
                    };
                    effects.push(self.filing.write(&mut job, AfterWrite::Result, write));
                }
                Outcome::Posted(outcome) => {
                    let action = job.action.as_ref().expect("a create has an action");
                    let write = Write::Posted {
                        draft: action.draft,
                        at: action.version,
                        outcome,
                    };
                    effects.push(self.filing.write(&mut job, AfterWrite::Result, write));
                }
                Outcome::Built(Err(_)) => {
                    effects.push(self.issue_unsent(&mut job, FailReason::PreflightFailed));
                }
                Outcome::Stopped => effects.push(self.issue_interrupted(&mut job)),
                Outcome::Deadline | Outcome::ReviewDeadline => {
                    unreachable!("deadlines returned above")
                }
            }
        }
        self.filing.job = Some(job);
        effects
    }

    fn issue_send(&mut self, job: &mut Job) -> Option<Effect> {
        let prepared = job
            .prepared
            .take()
            .expect("Posting follows complete preflight evidence");
        let action = job.action.as_ref().expect("Posting has an action");
        let Some((token, until, expiry)) =
            self.account
                .github
                .issue_permit(job.stamp, action, &prepared.user, &prepared.repos)
        else {
            return Some(self.issue_unsent(job, FailReason::NotConnected));
        };
        let http =
            prepared
                .http
                .with_post_access(until, expiry, Arc::clone(&self.account.net.clock));
        let action = action.clone();
        job.phase = Phase::Network(tokio::spawn(async move {
            Outcome::Posted(
                create_issue(
                    &http,
                    &token,
                    action.repo.as_ref().expect("Posting is bound"),
                    IssueContent {
                        title: &action.title,
                        body: &action.body,
                    },
                )
                .await,
            )
        }));
        None
    }

    fn issue_unsent(&mut self, job: &mut Job, reason: FailReason) -> Effect {
        let action = job
            .action
            .as_ref()
            .expect("a network phase has a durable action");
        let write = if job.check {
            Write::LookedUp {
                draft: action.draft,
                episode: action.uncertain,
                lookup: Lookup::Failed,
            }
        } else {
            Write::NotSent {
                draft: action.draft,
                at: action.version,
                reason,
            }
        };
        self.filing.write(job, AfterWrite::Cleanup, write)
    }

    fn issue_interrupted(&mut self, job: &mut Job) -> Effect {
        if !job.control.may_have_sent() {
            return self.issue_unsent(job, FailReason::Interrupted);
        }
        let action = job
            .action
            .as_ref()
            .expect("a handed create has a durable action");
        let write = Write::Interrupted {
            draft: action.draft,
            at: action.version,
        };
        self.filing.write(job, AfterWrite::Cleanup, write)
    }

    fn issue_unavailable(&self, epoch: u64, request: u64, draft: DraftId) -> Vec<Effect> {
        self.issue_unavailable_kind(epoch, request, draft, IssueUnavailableKind::Action)
    }

    fn issue_unavailable_kind(
        &self,
        epoch: u64,
        request: u64,
        draft: DraftId,
        kind: IssueUnavailableKind,
    ) -> Vec<Effect> {
        if epoch != self.epoch {
            return Vec::new();
        }
        vec![Effect::Control(ControlToShell::IssueUnavailable {
            request,
            draft,
            kind,
        })]
    }

    fn issue_notice(
        &self,
        epoch: u64,
        request: Option<u64>,
        draft: DraftId,
        answer: &Answer,
        forced: Option<IssueRefused>,
    ) -> Vec<Effect> {
        let current = epoch == self.epoch;
        let request = request.filter(|_| current);
        let (action, reason) = match answer {
            Answer::Changed(action) | Answer::Unchanged(action) => (action, forced),
            Answer::Refused(reason, action) => (action, forced.or(Some(*reason))),
            Answer::Gone => {
                return request.map_or_else(Vec::new, |request| {
                    self.issue_unavailable(epoch, request, draft)
                });
            }
        };
        let mut effects = vec![Effect::Control(ControlToShell::ActionState {
            request,
            draft: action.draft,
            version: action.version,
            state: action.state(),
            approval: action.approval,
        })];
        if let Some(reason) = reason.filter(|_| current) {
            effects.push(Effect::Control(ControlToShell::IssueRefused {
                request: request.expect("a refusal settles its operation"),
                draft: action.draft,
                version: action.version,
                approval: action.approval,
                reason,
            }));
        }
        effects
    }

    pub(super) async fn finish_filing(&mut self, deadline: Instant) -> bool {
        if !self.filing.pending() {
            return true;
        }
        self.filing.cancel();
        let in_time = tokio::time::timeout_at(deadline, self.drain_filing())
            .await
            .is_ok()
            && Instant::now() <= deadline;
        if !in_time {
            tracing::warn!("an issue phase remains at the shutdown deadline; retaining its owner");
            self.drain_filing().await;
        }
        in_time
    }

    async fn drain_filing(&mut self) {
        while self.filing.pending() {
            let effects = tokio::select! {
                Some(input) = self.inputs.recv() => self.handle(input).unwrap_or_default(),
                event = self.filing.next() => self.issue_event(event),
            };
            self.apply(effects, None)
                .expect("offline issue cleanup cannot fill a shell queue");
        }
    }
}

fn stopped(job: &Job) -> bool {
    job.control.cancelled() || Instant::now() >= job.control.deadline()
}
