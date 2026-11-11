use super::{Accounts, Event, HttpFactory, Work};
use crate::connections::github::{
    GitHubToken,
    api::{self, ApiError, GitHubUser},
    device::{Effect, Input, Machine, Stamp},
    lane::{Job, LoadError, Reply},
};
use crate::connections::http_client::{HttpError, SharedHttp};
use kumochi_protocol::Secret;
use kumochi_protocol::message::{ControlToShell, ErrorKey, GitHubOp};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::Sender,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationStage {
    Construction,
    Request,
}

#[derive(Clone)]
pub(crate) enum VerificationOutcome {
    Built(Result<SharedHttp, HttpError>),
    User(Result<GitHubUser, ApiError>),
    Stopped,
}

impl std::fmt::Debug for VerificationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Built(_) => "GitHubClientBuilt",
            Self::User(_) => "GitHubUserResult",
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
    Request(JoinHandle<Option<Result<GitHubUser, ApiError>>>),
    Completed(VerificationStage),
}

struct Verification {
    id: u64,
    stamp: Stamp,
    session: Option<u64>,
    cancelled: bool,
    token: Option<Secret<GitHubToken>>,
    phase: VerificationPhase,
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
    verify_loaded: bool,
    next_verification: u64,
    verification: Option<Verification>,
    machine: Machine,
    session: Option<u64>,
    closing: bool,
    outstanding: usize,
    suspended: Vec<(Stamp, Effect)>,
}

impl std::fmt::Debug for GitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitHubOwner")
    }
}

impl GitHub {
    pub(super) fn new() -> Self {
        Self {
            enabled: false,
            verify_loaded: false,
            next_verification: 0,
            verification: None,
            machine: Machine::new(None),
            session: None,
            closing: false,
            outstanding: 0,
            suspended: Vec::new(),
        }
    }

    pub(super) fn start(
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
            verification.token = None;
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
        let Some(verification) = &mut self.verification else {
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
                    Ok(Some(result)) => VerificationOutcome::User(result),
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

    pub(super) fn verified(
        &mut self,
        event: VerificationEvent,
        jobs: Option<&Sender<Work>>,
        events: &mpsc::UnboundedSender<Event>,
        http: &HttpFactory,
    ) -> Vec<ControlToShell> {
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
            || !self.verify_loaded
            || self.closing
            || verification.cancelled
            || verification.stamp != self.machine.stamp()
        {
            return Vec::new();
        }
        match event.outcome {
            VerificationOutcome::Built(Ok(client)) => {
                let token = verification
                    .token
                    .take()
                    .expect("eligible construction owns its token");
                verification.phase = VerificationPhase::Request(tokio::spawn(read_user(
                    verification.stamp,
                    self.machine.fence(),
                    token,
                    client,
                )));
                self.verification = Some(verification);
                Vec::new()
            }
            VerificationOutcome::Built(Err(error)) => self.input(
                Input::User {
                    stamp: event.stamp,
                    result: Err(ApiError::Http(error)),
                },
                event.session,
                jobs,
                events,
                http,
            ),
            VerificationOutcome::User(result) => self.input(
                Input::User {
                    stamp: event.stamp,
                    result,
                },
                event.session,
                jobs,
                events,
                http,
            ),
            VerificationOutcome::Stopped => {
                if event.session.is_some() && event.session == self.session {
                    vec![ControlToShell::GitHubError(ErrorKey::RequestRefused.into())]
                } else {
                    Vec::new()
                }
            }
        }
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
        if self.enabled {
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
        self.session(None, jobs, events, http);
        self.cancel_verification();
        self.suspended.clear();
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
        self.suspended
            .retain(|(stamp, effect)| !match (&input, effect) {
                (Input::Code { stamp: done, .. }, Effect::RequestCode { .. })
                | (Input::Poll { stamp: done, .. }, Effect::Poll { .. })
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
                    self.cancel_verification();
                }
                Effect::ReadUser { stamp, token } => {
                    if !self.closing {
                        if self.verify_loaded && self.verification.is_none() {
                            self.next_verification = self
                                .next_verification
                                .checked_add(1)
                                .expect("verification identities never wrap");
                            let build = Arc::clone(http);
                            self.verification = Some(Verification {
                                id: self.next_verification,
                                stamp,
                                session: origin,
                                cancelled: false,
                                token: Some(token),
                                phase: VerificationPhase::Construction(
                                    tokio::task::spawn_blocking(move || build()),
                                ),
                            });
                        } else {
                            self.suspended
                                .push((stamp, Effect::ReadUser { stamp, token }));
                        }
                    }
                }
                effect @ (Effect::RequestCode { stamp } | Effect::Poll { stamp, .. }) => {
                    if !self.closing {
                        self.suspended.push((stamp, effect));
                    }
                }
                effect @ (Effect::Wake(_) | Effect::SaveDefault(_)) => {
                    if !self.closing {
                        self.suspended.push((self.machine.stamp(), effect));
                    }
                }
                effect @ Effect::RefreshRepos { generation } => {
                    if !self.closing {
                        self.suspended.push((Stamp { generation }, effect));
                    }
                }
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
    pub(crate) fn suspended(&self) -> &[(Stamp, Effect)] {
        &self.suspended
    }

    #[cfg(test)]
    pub(crate) fn enable(&mut self) {
        self.enabled = true;
    }

    #[cfg(test)]
    pub(crate) fn enable_verification(&mut self) {
        self.verify_loaded = true;
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

impl Accounts {
    pub(crate) fn github_session(&mut self, session: Option<u64>) {
        self.github
            .session(session, self.jobs.as_ref(), &self.reads_tx, &self.net.http);
    }

    pub(crate) fn github_opened(&self) -> Vec<ControlToShell> {
        self.github.opened()
    }
}
