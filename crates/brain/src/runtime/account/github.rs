use super::{Accounts, Brain, Event, Full, HttpFactory, Link, Work, settings};
use crate::config::tuning::GitHubTuning;
use crate::connections::github::{
    DeviceCodeValue, GitHubToken, RefreshToken,
    api::{self, ApiError, GitHubUser},
    device::{DefaultRepo, Effect, Input, Machine, Repositories, Stamp},
    lane::{Job, LoadError, Reply},
};
use crate::connections::http_client::{Http, HttpError, Request, Response, SharedHttp};
use kumochi_protocol::Secret;
use kumochi_protocol::message::{ControlToShell, ErrorKey, GitHubOp, RepoName};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
    mpsc::Sender,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub(in crate::runtime) mod repositories;

#[cfg(test)]
pub(in crate::runtime) mod draft_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationStage {
    Construction,
    Request,
    Wake,
}

#[derive(Clone)]
pub(crate) enum VerificationOutcome {
    Built(Result<SharedHttp, HttpError>),
    User(Result<GitHubUser, ApiError>),
    Code(Result<api::DeviceCode, ApiError>),
    Poll(Result<api::TokenReply, ApiError>),
    Refreshed(Result<api::Tokens, ApiError>),
    Repositories(Result<Repositories, ApiError>),
    Wake,
    Stopped,
}

impl std::fmt::Debug for VerificationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Built(_) => "GitHubClientBuilt",
            Self::User(_) => "GitHubUserResult",
            Self::Code(_) => "GitHubCodeResult",
            Self::Poll(_) => "GitHubPollResult",
            Self::Refreshed(_) => "GitHubRefreshResult",
            Self::Repositories(_) => "GitHubRepositoriesResult",
            Self::Wake => "GitHubWake",
            Self::Stopped => "GitHubVerificationStopped",
        })
    }
}

#[derive(Clone)]
pub(crate) struct VerificationEvent {
    pub(super) id: u64,
    pub(super) stamp: Stamp,
    pub(super) session: Option<u64>,
    pub(super) stage: VerificationStage,
    pub(super) outcome: VerificationOutcome,
}

impl std::fmt::Debug for VerificationEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitHubVerificationEvent")
    }
}

enum VerificationPhase {
    Construction(JoinHandle<Result<SharedHttp, HttpError>>),
    Request(JoinHandle<Option<VerificationOutcome>>),
    Completed(VerificationStage),
}

struct Verification {
    id: u64,
    stamp: Stamp,
    session: Option<u64>,
    cancelled: bool,
    operation: Option<Operation>,
    kind: OperationKind,
    phase: VerificationPhase,
}

#[cfg(test)]
enum Execution {
    Manual,
    VerificationOnly,
    Automatic,
}

#[derive(Clone, Copy)]
enum OperationKind {
    User,
    Code,
    Poll,
    Refresh,
    Repositories,
}

enum Operation {
    User(Secret<GitHubToken>),
    Code,
    Poll(Secret<DeviceCodeValue>),
    Refresh(Secret<RefreshToken>),
    Repositories(Secret<GitHubToken>),
}

impl Operation {
    fn kind(&self) -> OperationKind {
        match self {
            Self::User(_) => OperationKind::User,
            Self::Code => OperationKind::Code,
            Self::Poll(_) => OperationKind::Poll,
            Self::Refresh(_) => OperationKind::Refresh,
            Self::Repositories(_) => OperationKind::Repositories,
        }
    }
}

impl OperationKind {
    fn failed(self, stamp: Stamp, error: ApiError) -> Input {
        match self {
            Self::User => Input::User {
                stamp,
                result: Err(error),
            },
            Self::Code => Input::Code {
                stamp,
                result: Err(error),
            },
            Self::Poll => Input::Poll {
                stamp,
                result: Err(error),
            },
            Self::Refresh => Input::Refreshed {
                stamp,
                result: Err(error),
            },
            Self::Repositories => Input::Repos {
                generation: stamp.generation,
                result: Err(error),
            },
        }
    }
}

#[derive(Clone, Copy)]
struct Wake {
    id: u64,
    stamp: Stamp,
    session: Option<u64>,
    at: Instant,
}

impl std::fmt::Debug for Verification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitHubVerification")
    }
}

pub(super) async fn read_user(
    stamp: Stamp,
    fence: Arc<AtomicU64>,
    token: Secret<GitHubToken>,
    http: SharedHttp,
) -> Option<Result<GitHubUser, ApiError>> {
    if fence.load(Ordering::Acquire) != stamp.generation {
        return None;
    }
    Some(api::user(&http, &token).await)
}

pub(crate) struct GitHub {
    enabled: bool,
    next_verification: u64,
    verification: Option<Verification>,
    wake: Option<Wake>,
    client_id: Option<&'static str>,
    defaults: Option<(u64, DefaultRepo)>,
    #[cfg(test)]
    execution: Execution,
    machine: Machine,
    session: Option<u64>,
    closing: bool,
    rotation_stopped: bool,
    outstanding: usize,
    suspended: Vec<(Stamp, Effect)>,
    issue: Option<Weak<crate::runtime::issues::transport::Control>>,
    /// Filing and listing budgets from `[tuning.github]`, set once at start.
    pub(in crate::runtime) tuning: GitHubTuning,
}

impl std::fmt::Debug for GitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitHubOwner")
    }
}

impl GitHub {
    pub(super) fn new() -> Self {
        Self::with_client(kumochi_protocol::constants::GITHUB_CLIENT_ID)
    }

    pub(in crate::runtime) fn with_client(client_id: Option<&'static str>) -> Self {
        let client_id =
            client_id.filter(|value| crate::connections::github::ClientId::new(value).is_ok());
        Self {
            enabled: client_id.is_some(),
            next_verification: 0,
            verification: None,
            wake: None,
            client_id,
            defaults: None,
            #[cfg(test)]
            execution: Execution::Automatic,
            machine: Machine::new(None),
            session: None,
            closing: false,
            rotation_stopped: false,
            outstanding: 0,
            suspended: Vec::new(),
            tuning: GitHubTuning::default(),
            issue: None,
        }
    }

    pub(in crate::runtime) fn start(
        &mut self,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
    ) {
        if self.enabled && !self.closing {
            self.submit(
                Job::Load {
                    stamp: self.machine.stamp(),
                },
                jobs,
                events,
            );
        }
    }

    pub(crate) fn pending(&self) -> bool {
        self.outstanding != 0
    }

    pub(crate) fn verifying(&self) -> bool {
        self.verification.is_some()
    }

    fn cancel_verification(&mut self) {
        if let Some(verification) = &mut self.verification {
            verification.cancelled = true;
            verification.operation = None;
            if let VerificationPhase::Request(task) = &verification.phase {
                task.abort();
            }
        }
        if self.verification.as_ref().is_some_and(|verification| {
            matches!(verification.phase, VerificationPhase::Completed(_))
        }) {
            self.verification = None;
        }
    }

    pub(super) async fn next_verification(&mut self) -> VerificationEvent {
        let wake = self.wake;
        tokio::select! {
            event = next_request(&mut self.verification) => event,
            event = async move {
                let Some(wake) = wake else {
                    return std::future::pending().await;
                };
                tokio::time::sleep_until(wake.at).await;
                VerificationEvent {
                    id: wake.id,
                    stamp: wake.stamp,
                    session: wake.session,
                    stage: VerificationStage::Wake,
                    outcome: VerificationOutcome::Wake,
                }
            } => event,
        }
    }

    pub(super) fn verified(
        &mut self,
        event: VerificationEvent,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        if event.stage == VerificationStage::Wake {
            if !self.wake.is_some_and(|wake| {
                wake.id == event.id && wake.stamp == event.stamp && wake.session == event.session
            }) || event.stamp != self.machine.stamp()
            {
                return Vec::new();
            }
            self.wake = None;
            return self.input(Input::Tick, event.session, jobs, events, http);
        }
        if !self.verification.as_ref().is_some_and(|verification| {
            verification.id == event.id && verification.stamp == event.stamp
                && verification.session == event.session
                && matches!(verification.phase, VerificationPhase::Completed(stage) if stage == event.stage)
        }) {
            return Vec::new();
        }
        let mut verification = self
            .verification
            .take()
            .expect("the completed operation matched");
        if !self.enabled
            || !self.verifies_loaded()
            || (self.closing && !self.machine.rotating())
            || self.rotation_stopped
            || verification.cancelled
            || verification.stamp != self.machine.stamp()
        {
            return self.resume_suspended(jobs, events, http);
        }
        let input = match event.outcome {
            VerificationOutcome::Built(Ok(client)) => {
                let operation = verification
                    .operation
                    .take()
                    .expect("eligible construction owns its operation");
                verification.phase = VerificationPhase::Request(tokio::spawn(execute(
                    verification.stamp,
                    self.machine.fence(),
                    operation,
                    client,
                    self.client_id
                        .expect("configured operations have a client identifier"),
                    self.tuning,
                )));
                self.verification = Some(verification);
                return Vec::new();
            }
            VerificationOutcome::Built(Err(error)) => {
                verification.kind.failed(event.stamp, ApiError::Http(error))
            }
            VerificationOutcome::User(result) => Input::User {
                stamp: event.stamp,
                result,
            },
            VerificationOutcome::Code(result) => Input::Code {
                stamp: event.stamp,
                result,
            },
            VerificationOutcome::Poll(result) => Input::Poll {
                stamp: event.stamp,
                result,
            },
            VerificationOutcome::Refreshed(result) => Input::Refreshed {
                stamp: event.stamp,
                result,
            },
            VerificationOutcome::Repositories(result) => Input::Repos {
                generation: event.stamp.generation,
                result,
            },
            VerificationOutcome::Stopped => Input::Stopped { stamp: event.stamp },
            VerificationOutcome::Wake => return Vec::new(),
        };
        self.input(input, event.session, jobs, events, http)
    }

    fn automatic(&self) -> bool {
        #[cfg(test)]
        return matches!(self.execution, Execution::Automatic);
        #[cfg(not(test))]
        self.enabled
    }

    fn verifies_loaded(&self) -> bool {
        #[cfg(test)]
        return !matches!(self.execution, Execution::Manual);
        #[cfg(not(test))]
        self.enabled
    }

    fn resume_suspended(
        &mut self,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        if !self.automatic() || self.verification.is_some() {
            return Vec::new();
        }
        let pending = std::mem::take(&mut self.suspended);
        let effects = pending
            .into_iter()
            .filter_map(|(stamp, effect)| (stamp == self.machine.stamp()).then_some(effect))
            .collect();
        self.apply(effects, self.session, jobs, events, http)
    }

    pub(super) fn opened(&self) -> Vec<ControlToShell> {
        if !self.enabled || self.session.is_none() || self.closing || self.machine.is_loading() {
            return Vec::new();
        }
        vec![ControlToShell::GitHub(self.machine.view(Instant::now()))]
    }

    pub(super) fn session(
        &mut self,
        session: Option<u64>,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) {
        if session.is_some() && session == self.session {
            return;
        }
        self.session = None;
        self.cancel_issue();
        if self.enabled && self.machine.connecting() {
            self.input(Input::Op(GitHubOp::CancelConnect), None, jobs, events, http);
        }
        self.session = session;
    }

    pub(super) fn stop(
        &mut self,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) {
        self.closing = true;
        self.cancel_issue();
        self.session(None, jobs, events, http);
        self.wake = None;
        if !self.machine.rotating() {
            self.cancel_verification();
            self.suspended.clear();
        }
    }

    pub(super) fn rotating(&self) -> bool {
        self.machine.rotating() && !self.rotation_stopped
    }

    pub(super) fn stop_rotation(&mut self) {
        self.rotation_stopped = true;
        self.cancel_verification();
        self.suspended.clear();
        self.wake = None;
    }

    pub(in crate::runtime) fn restore_default(&mut self, default: Option<DefaultRepo>) {
        self.machine.restore_default(default);
    }

    pub(in crate::runtime) fn issue_snapshot(&self) -> crate::issues::Snapshot {
        let usable = self.enabled && !self.closing;
        crate::issues::Snapshot {
            capacity: false,
            account: usable.then(|| self.machine.user().cloned()).flatten(),
            repos: if usable {
                self.machine.repositories().unwrap_or_default().to_vec()
            } else {
                Vec::new()
            },
        }
    }

    pub(in crate::runtime) fn watch_issue(
        &mut self,
        control: &Arc<crate::runtime::issues::transport::Control>,
    ) -> Stamp {
        self.issue = Some(Arc::downgrade(control));
        self.machine.stamp()
    }

    pub(in crate::runtime) fn issue_token(
        &self,
        stamp: Stamp,
        action: &crate::issues::Action,
    ) -> Option<Secret<GitHubToken>> {
        if !self.enabled || self.closing || stamp != self.machine.stamp() {
            return None;
        }
        let (token, _, _) = self
            .machine
            .issue_access(action.account.as_ref()?.id, action.repo.as_ref()?)?;
        Some(token)
    }

    pub(in crate::runtime) fn issue_permit(
        &self,
        stamp: Stamp,
        action: &crate::issues::Action,
        user: &GitHubUser,
        repos: &Repositories,
    ) -> Option<(Secret<GitHubToken>, Instant, u64)> {
        if !self.enabled
            || self.closing
            || stamp != self.machine.stamp()
            || action.account.as_ref()?.id != user.id
            || !repos.names.as_ref().contains(action.repo.as_ref()?)
            || self.issue.as_ref()?.upgrade()?.cancelled()
        {
            return None;
        }
        self.machine.issue_access(user.id, action.repo.as_ref()?)
    }

    fn cancel_issue(&mut self) {
        if let Some(control) = self.issue.take().and_then(|control| control.upgrade()) {
            control.cancel();
        }
    }

    pub(in crate::runtime) fn draft_binding(
        &self,
        proposed: Option<&RepoName>,
        default: Option<DefaultRepo>,
    ) -> Result<(Option<RepoName>, Option<GitHubUser>), crate::dispatch::ToolError> {
        if !self.enabled || self.closing {
            return Ok((proposed.cloned(), None));
        }
        let (Some(user), Some(repos)) = (self.machine.user(), self.machine.repositories()) else {
            return Ok((proposed.cloned(), None));
        };
        let repo = if let Some(proposed) = proposed {
            if !repos.contains(proposed) {
                return Err(crate::dispatch::ToolError::NotHere);
            }
            Some(proposed.clone())
        } else {
            default
                .filter(|default| default.account == user.id && repos.contains(&default.repo))
                .map(|default| default.repo)
        };
        Ok((repo, Some(user.clone())))
    }

    pub(super) fn take_default(&mut self) -> Option<(u64, DefaultRepo)> {
        self.defaults.take()
    }

    pub(super) fn op(
        &mut self,
        op: GitHubOp,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        if matches!(op, GitHubOp::Connect) && self.client_id.is_none() {
            return vec![ControlToShell::GitHubError(
                ErrorKey::AccountMisconfigured.into(),
            )];
        }
        if !self.enabled || self.closing {
            return vec![ControlToShell::GitHubError(
                ErrorKey::GitHubUnavailable.into(),
            )];
        }
        if matches!(op, GitHubOp::Connect | GitHubOp::RefreshRepos) && self.verification.is_some() {
            return vec![ControlToShell::GitHubError(ErrorKey::RequestRefused.into())];
        }
        self.input(Input::Op(op), self.session, jobs, events, http)
    }

    pub(super) fn replied(
        &mut self,
        session: Option<u64>,
        reply: Reply,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        self.outstanding = self
            .outstanding
            .checked_sub(1)
            .expect("an admitted credential reply");
        let input = match reply {
            Reply::Loaded {
                stamp,
                result: Err(LoadError::Unavailable),
            } => {
                return if self.enabled
                    && !self.closing
                    && self.session.is_some()
                    && session == self.session
                    && stamp == self.machine.stamp()
                    && self.machine.is_loading()
                {
                    vec![ControlToShell::GitHubError(
                        ErrorKey::CredentialsUnavailable.into(),
                    )]
                } else {
                    Vec::new()
                };
            }
            Reply::Loaded {
                stamp,
                result: Err(LoadError::Cleanup(result)),
            } => Input::Loaded {
                stamp,
                result: Err(result),
            },
            Reply::Loaded {
                stamp,
                result: Ok(account),
            } => Input::Loaded {
                stamp,
                result: Ok(account),
            },
            Reply::Saved { stamp, saved } => Input::Saved { stamp, saved },
            Reply::Removed { generation, result } => Input::Removed { generation, result },
        };
        self.input(input, session, jobs, events, http)
    }

    fn input(
        &mut self,
        input: Input,
        origin: Option<u64>,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        if !self.enabled {
            return Vec::new();
        }
        let current = match &input {
            Input::Loaded { stamp, .. }
            | Input::Code { stamp, .. }
            | Input::Poll { stamp, .. }
            | Input::Refreshed { stamp, .. }
            | Input::User { stamp, .. }
            | Input::Saved { stamp, .. }
            | Input::Stopped { stamp } => *stamp == self.machine.stamp(),
            Input::Removed { generation, .. }
            | Input::Repos { generation, .. }
            | Input::Unauthorized { generation } => *generation == self.machine.stamp().generation,
            Input::DefaultSaved { .. } => false,
            Input::Op(_) | Input::Tick => true,
        };
        if current {
            self.cancel_issue();
        }
        self.suspended
            .retain(|(stamp, effect)| !match (&input, effect) {
                (Input::Code { stamp: done, .. }, Effect::RequestCode { .. })
                | (Input::Poll { stamp: done, .. }, Effect::Poll { .. })
                | (Input::Refreshed { stamp: done, .. }, Effect::RefreshToken { .. })
                | (Input::User { stamp: done, .. }, Effect::ReadUser { .. }) => done == stamp,
                (Input::Repos { generation, .. }, Effect::RefreshRepos { .. }) => {
                    *generation == stamp.generation
                }
                _ => false,
            });
        let effects = self.machine.on(input, Instant::now());
        self.suspended
            .retain(|(stamp, _)| *stamp == self.machine.stamp());
        self.apply(effects, origin, jobs, events, http)
    }

    fn apply(
        &mut self,
        effects: Vec<Effect>,
        origin: Option<u64>,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        if !self.enabled {
            return Vec::new();
        }
        let mut frames = Vec::new();
        for effect in effects {
            match effect {
                Effect::Save { stamp, account } => self.submit(
                    Job::Save {
                        stamp,
                        fence: self.machine.fence(),
                        account,
                    },
                    jobs,
                    events,
                ),
                Effect::Remove { generation } => {
                    self.submit(Job::Remove { generation }, jobs, events);
                }
                Effect::CancelAll => {
                    self.suspended.clear();
                    self.wake = None;
                    self.defaults = None;
                    self.cancel_verification();
                }
                effect @ (Effect::ReadUser { .. }
                | Effect::RequestCode { .. }
                | Effect::Poll { .. }
                | Effect::RefreshToken { .. }
                | Effect::RefreshRepos { .. }) => {
                    if !self.closing || (self.machine.rotating() && !self.rotation_stopped) {
                        frames.extend(self.start_effect(effect, origin, jobs, events, http));
                    }
                }
                Effect::Wake(at) if !self.closing => {
                    if self.automatic() {
                        self.next_verification = self
                            .next_verification
                            .checked_add(1)
                            .expect("operation identities never wrap");
                        self.wake = Some(Wake {
                            id: self.next_verification,
                            stamp: self.machine.stamp(),
                            session: origin,
                            at,
                        });
                    } else {
                        self.suspended
                            .push((self.machine.stamp(), Effect::Wake(at)));
                    }
                }
                Effect::SaveDefault {
                    generation,
                    default,
                } if !self.closing => {
                    self.defaults = Some((generation, default));
                }
                Effect::Wake(_) | Effect::SaveDefault { .. } => {}
                Effect::View(view) => {
                    if self.enabled
                        && !self.closing
                        && self.session.is_some()
                        && !self.machine.is_loading()
                    {
                        frames.push(ControlToShell::GitHub(view));
                    }
                }
                Effect::Error(error) => {
                    if self.enabled && !self.closing && origin.is_some() && origin == self.session {
                        frames.push(ControlToShell::GitHubError(error));
                    }
                }
            }
        }
        frames
    }

    fn start_effect(
        &mut self,
        effect: Effect,
        origin: Option<u64>,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
        let user = matches!(effect, Effect::ReadUser { .. });
        if self.verification.is_some() || !self.verifies_loaded() || (!user && !self.automatic()) {
            self.suspended.push((self.machine.stamp(), effect));
            return Vec::new();
        }
        let (stamp, operation) = match effect {
            Effect::RequestCode { stamp } => (stamp, Operation::Code),
            Effect::ReadUser { stamp, token } => (stamp, Operation::User(token)),
            Effect::Poll { stamp, code } => (stamp, Operation::Poll(code)),
            Effect::RefreshToken { stamp, token } => (stamp, Operation::Refresh(token)),
            Effect::RefreshRepos { generation } => {
                let Some(token) = self.machine.token() else {
                    return self.input(
                        Input::Repos {
                            generation,
                            result: Err(ApiError::Response),
                        },
                        origin,
                        jobs,
                        events,
                        http,
                    );
                };
                (Stamp { generation }, Operation::Repositories(token))
            }
            _ => unreachable!("only network effects are dispatched"),
        };
        self.next_verification = self
            .next_verification
            .checked_add(1)
            .expect("operation identities never wrap");
        let build = Arc::clone(http);
        self.verification = Some(Verification {
            id: self.next_verification,
            stamp,
            session: origin,
            cancelled: false,
            kind: operation.kind(),
            operation: Some(operation),
            phase: VerificationPhase::Construction(tokio::task::spawn_blocking(move || build())),
        });
        Vec::new()
    }

    fn submit(
        &mut self,
        job: Job,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
    ) {
        let unavailable = match &job {
            Job::Load { stamp } => Reply::Loaded {
                stamp: *stamp,
                result: Err(LoadError::Unavailable),
            },
            Job::Save { stamp, .. } => Reply::Saved {
                stamp: *stamp,
                saved: false,
            },
            Job::Remove { generation } => Reply::Removed {
                generation: *generation,
                result: crate::connections::github::device::Removed::NotDurable,
            },
        };
        let (reply, receiver) = oneshot::channel();
        let session = self.session;
        self.outstanding = self
            .outstanding
            .checked_add(1)
            .expect("credential replies fit usize");
        if jobs.is_some_and(|jobs| jobs.send(Work::GitHub { job, reply }).is_ok()) {
            let events = events.clone();
            drop(tokio::spawn(async move {
                let reply = receiver.await.unwrap_or(unavailable);
                events.send(Event::GitHub { session, reply }).ok();
            }));
        } else {
            events
                .send(Event::GitHub {
                    session,
                    reply: unavailable,
                })
                .ok();
        }
    }

    #[cfg(test)]
    pub(crate) fn machine(&self) -> &Machine {
        &self.machine
    }

    #[cfg(test)]
    pub(crate) fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    #[cfg(test)]
    pub(crate) fn suspended(&self) -> &[(Stamp, Effect)] {
        &self.suspended
    }

    #[cfg(test)]
    pub(crate) fn enable(&mut self) {
        self.enabled = true;
        self.client_id = Some("fixture-client");
        self.execution = Execution::Manual;
    }

    #[cfg(test)]
    pub(crate) fn enable_verification(&mut self) {
        self.execution = Execution::VerificationOnly;
    }

    #[cfg(test)]
    pub(crate) fn wake(
        &mut self,
        stamp: Stamp,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Result<Vec<ControlToShell>, ()> {
        if self.closing || stamp != self.machine.stamp() {
            return Err(());
        }
        let Some(at) = self.suspended.iter().position(|(owner, effect)| {
            *owner == stamp && matches!(effect, Effect::Wake(due) if *due <= Instant::now())
        }) else {
            return Err(());
        };
        drop(self.suspended.remove(at));
        Ok(self.input(Input::Tick, self.session, jobs, events, http))
    }

    #[cfg(test)]
    pub(crate) fn checked(
        &mut self,
        input: Input,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Result<Vec<ControlToShell>, ()> {
        if matches!(input, Input::Op(GitHubOp::SetDefaultRepo(_)) | Input::Tick) || self.closing {
            return Err(());
        }
        Ok(self.input(input, self.session, jobs, events, http))
    }
}

async fn next_request(owner: &mut Option<Verification>) -> VerificationEvent {
    let Some(verification) = owner else {
        return std::future::pending().await;
    };
    let (stage, outcome) = match &mut verification.phase {
        VerificationPhase::Construction(task) => (
            VerificationStage::Construction,
            match task.await {
                Ok(result) => VerificationOutcome::Built(result),
                Err(_) => VerificationOutcome::Stopped,
            },
        ),
        VerificationPhase::Request(task) => (
            VerificationStage::Request,
            match task.await {
                Ok(Some(result)) => result,
                Ok(None) | Err(_) => VerificationOutcome::Stopped,
            },
        ),
        VerificationPhase::Completed(_) => return std::future::pending().await,
    };
    verification.phase = VerificationPhase::Completed(stage);
    VerificationEvent {
        id: verification.id,
        stamp: verification.stamp,
        session: verification.session,
        stage,
        outcome,
    }
}

struct CurrentHttp {
    http: SharedHttp,
    fence: Arc<AtomicU64>,
    generation: u64,
}

impl Http for CurrentHttp {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        if self.fence.load(Ordering::Acquire) != self.generation {
            return Err(HttpError::Request);
        }
        self.http.execute(request).await
    }
}

async fn execute(
    stamp: Stamp,
    fence: Arc<AtomicU64>,
    operation: Operation,
    http: SharedHttp,
    client_id: &'static str,
    tuning: GitHubTuning,
) -> Option<VerificationOutcome> {
    if fence.load(Ordering::Acquire) != stamp.generation {
        return None;
    }
    if let Operation::User(token) = operation {
        return read_user(stamp, fence, token, http)
            .await
            .map(VerificationOutcome::User);
    }
    let http = CurrentHttp {
        http,
        fence,
        generation: stamp.generation,
    };
    Some(match operation {
        Operation::Code => {
            VerificationOutcome::Code(api::request_device_code(&http, client_id).await)
        }
        Operation::Poll(code) => {
            VerificationOutcome::Poll(api::poll_device_token(&http, client_id, &code).await)
        }
        Operation::Refresh(token) => {
            VerificationOutcome::Refreshed(api::refresh_user_token(&http, client_id, &token).await)
        }
        Operation::Repositories(token) => {
            VerificationOutcome::Repositories(repositories::fetch(&http, &token, tuning).await)
        }
        Operation::User(_) => unreachable!("user verification returned above"),
    })
}

impl Accounts {
    pub(in crate::runtime) fn issue_unauthorized(&mut self, stamp: Stamp) -> Vec<ControlToShell> {
        self.github.input(
            Input::Unauthorized {
                generation: stamp.generation,
            },
            self.github.session,
            self.jobs.as_ref(),
            &self.reads_tx,
            &self.net.http,
        )
    }

    pub(crate) fn github_session(&mut self, session: Option<u64>) {
        self.github
            .session(session, self.jobs.as_ref(), &self.reads_tx, &self.net.http);
    }

    pub(crate) fn github_opened(&self) -> Vec<ControlToShell> {
        self.github.opened()
    }
}

impl Brain {
    pub(in crate::runtime) fn github_op(
        &mut self,
        op: GitHubOp,
        mut link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let frames = self.account.github.op(
            op,
            self.account.jobs.as_ref(),
            &self.account.reads_tx,
            &self.account.net.http,
        );
        if let Some((generation, default)) = self.account.github.take_default() {
            self.settings_ops.push_back(settings::Queued {
                change: settings::Change::GitHub(settings::GitHubWrite {
                    generation,
                    default,
                }),
                epoch: self.epoch,
            });
            self.pump_settings(link.as_deref_mut())?;
        }
        if let Some(link) = link {
            for frame in frames {
                link.control(&frame)?;
            }
        }
        Ok(())
    }

    pub(in crate::runtime) fn github_default_written(
        &mut self,
        write: settings::GitHubWrite,
        result: Result<(), ErrorKey>,
        epoch: u64,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        if result.is_ok() {
            self.account
                .github
                .restore_default(self.config.github_default());
        }
        let frames = self.account.github.input(
            Input::DefaultSaved {
                generation: write.generation,
                repo: write.default.repo,
                saved: result.is_ok(),
            },
            Some(epoch),
            self.account.jobs.as_ref(),
            &self.account.reads_tx,
            &self.account.net.http,
        );
        if let Some(link) = link {
            for frame in frames {
                link.control(&frame)?;
            }
        }
        Ok(())
    }
}
