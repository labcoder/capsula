//! One Claude Code question on one launch (docs/development/claude-code.md, "A question"). The
//! core admits an [`Attempt`] and its audit task runs the audit before input; the round of the
//! [`ClaudeQuestion`] that takes it binds the run, asks to send, seeds and reads the answer.
//! Each attempt sends the core exactly one [`Outcome`], through its one [`Report`].

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::MCP_SERVER_NAME;
use kumochi_protocol::message::{AuditFailed, ClaudeVersion, ErrorKey, ThreadId};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use super::audit::{self, PostSeedAudit, PreInputAudit, Progress};
use super::policy::Reviewed;
use super::seed;
use super::stream::{LineFramer, Mapped, StreamMapper};
use super::supervisor::{
    Launch, LaunchFailed, LaunchRefused, Pipes, SETTLE, StdinRefused, StdinWriter,
};
use crate::bridge::LaunchId;
use crate::connections::model::budget::Budget;
use crate::context::{self, Origin};
use crate::engines::{
    Answer, ClaudeStreamFailure, Engine, EngineFailure, Finish, Round, RunRequest, SendGate,
    TurnEvent,
};

/// How long a warm launch waits for its question [guess, the design's 30 s].
pub(crate) const WARM: Duration = Duration::from_secs(30);
/// How long the audit before input may take once the control lines are queued (C3).
pub(crate) const AUDIT: Duration = Duration::from_secs(3);
/// How long a question may take from its bind.
pub(crate) const QUESTION: Duration = Duration::from_secs(120);

/// The clock warm expiry is judged by; tests set it.
#[derive(Clone)]
pub(crate) struct Clock(pub(crate) Arc<dyn Fn() -> Instant + Send + Sync>);

impl Clock {
    pub(crate) fn now(&self) -> Instant {
        (self.0)()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self(Arc::new(Instant::now))
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}

/// What a launch is fixed to at spawn; equal snapshots may share a launch. Its Debug shows the
/// model and effort by length only.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LaunchSnapshot {
    pub(crate) thread: ThreadId,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    /// Claude Code's connection generation.
    pub(crate) account: u64,
    /// The frozen tool set is `context::tools(origin)`.
    pub(crate) origin: Origin,
    pub(crate) version: ClaudeVersion,
}

impl fmt::Debug for LaunchSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchSnapshot")
            .field("thread", &self.thread)
            .field("model_bytes", &self.model.len())
            .field("effort_bytes", &self.effort.as_ref().map(String::len))
            .field("account", &self.account)
            .field("origin", &self.origin)
            .field("version", &self.version)
            .finish()
    }
}

/// How an attempt ended, as the core hears it once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The question ended normally, was cancelled, or was never asked.
    Clean,
    /// An audit failed: the version is quarantined.
    Audit(AuditFailed),
    /// Claude Code isn't signed in.
    SignedOut,
    /// A deadline passed.
    Deadline,
    /// stdout ended first.
    Eof,
    /// The launch never ran, or was revoked under the question.
    Launch,
    /// stdout broke its rules.
    Stream(ClaudeStreamFailure),
}

/// What attempts tell the core, on one channel it drains.
#[derive(Debug)]
pub(crate) enum Note {
    /// An attempt's one terminal report.
    Ended { id: LaunchId, outcome: Outcome },
    /// A seeded launch's analytics, once its init passed.
    Analytics { id: LaunchId, on: bool },
    /// A round whose warm attempt expired before bind wants a fresh one.
    Refresh(Refresh),
}

/// Sent by a round whose warm attempt expired before bind, which it has dropped already.
#[derive(Debug)]
pub(crate) struct Refresh {
    pub(crate) old: LaunchId,
    pub(crate) snapshot: LaunchSnapshot,
    pub(crate) reply: oneshot::Sender<Result<Attempt, LaunchRefused>>,
}

/// The attempt's one terminal report: [`report`](Self::report) consumes it, and its drop
/// sends [`Outcome::Clean`].
#[derive(Debug)]
pub(crate) struct Report(Option<(LaunchId, mpsc::UnboundedSender<Note>)>);

impl Report {
    pub(crate) fn new(id: LaunchId, notes: mpsc::UnboundedSender<Note>) -> Self {
        Self(Some((id, notes)))
    }

    pub(crate) fn report(mut self, outcome: Outcome) {
        self.send(outcome);
    }

    fn send(&mut self, outcome: Outcome) {
        if let Some((id, notes)) = self.0.take() {
            notes.send(Note::Ended { id, outcome }).ok();
        }
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        self.send(Outcome::Clean);
    }
}

/// Aborts its task when dropped.
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What the audit task hands over once the audit before input passed. The framer keeps every
/// byte read so far, the lines after the last control answer included.
struct Warmed {
    pipes: Pipes,
    writer: StdinWriter,
    framer: LineFramer,
    mapper: StreamMapper,
    post: PostSeedAudit,
    /// initialize's `analytics_disabled`.
    analytics: Option<bool>,
    report: Report,
}

/// One launch from admission until a round or a teardown ends it. Dropping it revokes the
/// launch and aborts its audit task.
pub(crate) struct Attempt {
    snapshot: LaunchSnapshot,
    expires: Option<Instant>,
    _audit: AbortOnDrop,
    ready: oneshot::Receiver<Result<Warmed, Outcome>>,
    launch: Launch,
}

impl fmt::Debug for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attempt")
            .field("id", &self.launch.id())
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl Attempt {
    /// Starts the audit task of `launch`, admitted for `snapshot`; `expires` is a warm's.
    pub(crate) fn new(
        snapshot: LaunchSnapshot,
        mut launch: Launch,
        reviewed: &'static Reviewed,
        report: Report,
        expires: Option<Instant>,
    ) -> Self {
        let tools = tool_names(snapshot.origin);
        let pre = PreInputAudit::new(reviewed, tools.clone());
        let post = PostSeedAudit::new(reviewed, tools);
        let (done, ready) = oneshot::channel();
        let task = tokio::spawn(warm_up(
            launch.started(),
            launch.deadline(),
            pre,
            post,
            report,
            done,
        ));
        Self {
            snapshot,
            expires,
            _audit: AbortOnDrop(task.abort_handle()),
            ready,
            launch,
        }
    }

    pub(crate) fn id(&self) -> LaunchId {
        self.launch.id()
    }

    pub(crate) fn snapshot(&self) -> &LaunchSnapshot {
        &self.snapshot
    }

    pub(crate) fn expires(&self) -> Option<Instant> {
        self.expires
    }

    async fn ready(&mut self) -> Result<Warmed, Outcome> {
        (&mut self.ready).await.unwrap_or(Err(Outcome::Launch))
    }

    /// The audit before input's hand-over, kept opaque.
    #[cfg(test)]
    pub(crate) async fn warmed(&mut self) -> Result<impl Sized, Outcome> {
        self.ready().await
    }
}

/// The launch's frozen `mcp__kumochi__*` names.
fn tool_names(origin: Origin) -> BTreeSet<String> {
    context::tools(origin)
        .iter()
        .map(|tool| format!("mcp__{MCP_SERVER_NAME}__{}", tool.name))
        .collect()
}

/// The audit task: it reports every failure itself, and on a pass hands its report on.
async fn warm_up(
    started: impl Future<Output = Option<Result<Pipes, LaunchFailed>>>,
    deadline: Instant,
    mut pre: PreInputAudit,
    post: PostSeedAudit,
    report: Report,
    done: oneshot::Sender<Result<Warmed, Outcome>>,
) {
    match audited(started, deadline, &mut pre).await {
        Ok((pipes, writer, framer, mapper)) => {
            let warmed = Warmed {
                pipes,
                writer,
                framer,
                mapper,
                post,
                analytics: pre.analytics(),
                report,
            };
            // Nobody to take it: its report drops as Clean.
            done.send(Ok(warmed)).ok();
        }
        Err(outcome) => {
            report.report(outcome);
            done.send(Err(outcome)).ok();
        }
    }
}

/// Writes the two control lines and reads stdout until the audit before input decides.
async fn audited(
    started: impl Future<Output = Option<Result<Pipes, LaunchFailed>>>,
    deadline: Instant,
    pre: &mut PreInputAudit,
) -> Result<(Pipes, StdinWriter, LineFramer, StreamMapper), Outcome> {
    let Some(Ok(mut pipes)) = started.await else {
        return Err(Outcome::Launch);
    };
    let mut writer = pipes.writer().ok_or(Outcome::Launch)?;
    for line in audit::control_requests() {
        writer
            .control(line, deadline)
            .await
            .map_err(|_| Outcome::Launch)?;
    }
    let until = deadline.min(Instant::now() + AUDIT);
    let mut framer = LineFramer::default();
    let mut mapper = StreamMapper::default();
    let read = async {
        loop {
            while let Some(line) = framer.next_line().map_err(Outcome::Stream)? {
                match mapper.line(line).map_err(Outcome::Stream)? {
                    Mapped::Ignored => {}
                    Mapped::Control(response) => match pre.control(&response) {
                        Ok(Progress::Waiting) => {}
                        Ok(Progress::Ready) => return Ok(()),
                        Ok(Progress::SignedOut) => return Err(Outcome::SignedOut),
                        Err(reason) => return Err(Outcome::Audit(reason)),
                    },
                    Mapped::Event(_) | Mapped::End { .. } | Mapped::Init(_) => {
                        return Err(Outcome::Audit(AuditFailed::Malformed));
                    }
                }
            }
            let chunk = pipes.stdout.recv().await.ok_or(Outcome::Eof)?;
            framer.push(&chunk).map_err(Outcome::Stream)?;
        }
    };
    tokio::time::timeout_at(until.into(), read)
        .await
        .unwrap_or(Err(Outcome::Deadline))?;
    Ok((pipes, writer, framer, mapper))
}

/// One question's engine: it owns one taken attempt, and borrows nothing of the core.
pub(crate) struct ClaudeQuestion {
    attempt: Mutex<Option<Attempt>>,
    budget: Arc<Budget>,
    notes: mpsc::UnboundedSender<Note>,
    clock: Clock,
}

impl ClaudeQuestion {
    pub(crate) fn new(
        attempt: Attempt,
        budget: Arc<Budget>,
        notes: mpsc::UnboundedSender<Note>,
        clock: Clock,
    ) -> Self {
        Self {
            attempt: Mutex::new(Some(attempt)),
            budget,
            notes,
            clock,
        }
    }
}

impl fmt::Debug for ClaudeQuestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeQuestion").finish_non_exhaustive()
    }
}

impl Engine for ClaudeQuestion {
    /// Takes the attempt out once; a second start fails at once.
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let attempt = self
            .attempt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let budget = Arc::clone(&self.budget);
        let notes = self.notes.clone();
        let clock = self.clock.clone();
        Box::pin(async move {
            let finish = match attempt {
                Some(attempt) => {
                    let round = Question {
                        events: &events,
                        gate: &gate,
                        budget: &budget,
                        notes: &notes,
                        clock: &clock,
                    };
                    round.play(attempt, &request).await
                }
                None => Some(Finish::Failed(EngineFailure::Provider)),
            };
            if let Some(finish) = finish {
                events.send(TurnEvent::Finished(finish)).await.ok();
            }
        })
    }
}

/// What a round borrows while it plays.
struct Question<'a> {
    events: &'a mpsc::Sender<TurnEvent>,
    gate: &'a SendGate,
    budget: &'a Budget,
    notes: &'a mpsc::UnboundedSender<Note>,
    clock: &'a Clock,
}

impl Question<'_> {
    /// The round's `Finished`; `None` for a `Rebuild`, which sends nothing.
    async fn play(&self, mut attempt: Attempt, request: &RunRequest) -> Option<Finish> {
        let mut warmed = match attempt.ready().await {
            Ok(warmed) => warmed,
            Err(outcome) => return Some(failed(outcome)),
        };
        if attempt
            .expires
            .is_some_and(|expires| self.clock.now() >= expires)
        {
            let deadline = attempt.launch.deadline();
            let (reply, fresh) = oneshot::channel();
            let refresh = Refresh {
                old: attempt.id(),
                snapshot: attempt.snapshot.clone(),
                reply,
            };
            drop(warmed);
            drop(attempt);
            if self.notes.send(Note::Refresh(refresh)).is_err() {
                return Some(Finish::Failed(EngineFailure::Provider));
            }
            attempt = match tokio::time::timeout_at(deadline.into(), fresh).await {
                Ok(Ok(Ok(attempt))) => attempt,
                _ => return Some(Finish::Failed(EngineFailure::Provider)),
            };
            warmed = match attempt.ready().await {
                Ok(warmed) => warmed,
                Err(outcome) => return Some(failed(outcome)),
            };
        }
        let Warmed {
            mut pipes,
            mut writer,
            mut framer,
            mut mapper,
            mut post,
            analytics,
            report,
        } = warmed;
        let launch = &attempt.launch;
        if launch.binding().bind(request.run).is_err() {
            report.report(Outcome::Launch);
            return Some(Finish::Failed(EngineFailure::Provider));
        }
        let until = launch.deadline().min(self.clock.now() + QUESTION);
        let hold = (launch.deadline() + SETTLE).saturating_duration_since(Instant::now());
        // A timeout polls the ask first, so a Grant ready after the deadline still needs refusing.
        let lease = match tokio::time::timeout_at(until.into(), self.gate.ask_held(hold)).await {
            Ok(Answer::Grant(lease)) if Instant::now() < until => lease,
            Ok(Answer::Rebuild) => return None,
            Ok(Answer::Grant(_)) | Err(_) => return Some(expired(launch, report)),
        };
        if !self.budget.reserve() {
            drop(lease);
            return Some(claude(ErrorKey::ClaudeCodeBudget));
        }
        if let Err(lease) = launch.hold_send(lease) {
            drop(lease);
            report.report(Outcome::Launch);
            return Some(Finish::Failed(EngineFailure::Provider));
        }
        let seed = seed::encode(&request.request, &context::tools(attempt.snapshot.origin));
        match writer.seed(&seed, until).await {
            Ok(()) => {}
            Err(StdinRefused::Deadline) => return Some(expired(launch, report)),
            Err(_) => {
                report.report(Outcome::Launch);
                return Some(Finish::Failed(EngineFailure::Provider));
            }
        }
        let answer = Reading {
            pipes: &mut pipes,
            framer: &mut framer,
            mapper: &mut mapper,
            post: &mut post,
            launch,
            analytics,
        };
        let read = tokio::time::timeout_at(until.into(), self.answer(answer))
            .await
            .unwrap_or(Err(Outcome::Deadline));
        match read {
            Ok((usage, finish)) => {
                writer.end();
                if let Some(usage) = usage {
                    self.events.send(TurnEvent::Usage(usage)).await.ok()?;
                }
                Some(finish)
            }
            Err(outcome) => {
                launch.lease().revoke();
                report.report(outcome);
                Some(failed(outcome))
            }
        }
    }

    /// Reads the answer: every init through the one post-seed audit, then the text and the end.
    async fn answer(
        &self,
        Reading {
            pipes,
            framer,
            mapper,
            post,
            launch,
            analytics,
        }: Reading<'_>,
    ) -> Result<(Option<crate::engines::Usage>, Finish), Outcome> {
        let mut audited = false;
        loop {
            while let Some(line) = framer.next_line().map_err(Outcome::Stream)? {
                match mapper.line(line).map_err(Outcome::Stream)? {
                    Mapped::Init(facts) => {
                        let record = post.init(facts).map_err(Outcome::Audit)?;
                        if !audited {
                            audited = true;
                            launch.gate().pass();
                            let on = !record.analytics_disabled.or(analytics).unwrap_or(true);
                            let id = launch.id();
                            self.notes.send(Note::Analytics { id, on }).ok();
                        }
                    }
                    Mapped::Ignored => {}
                    _ if !audited => return Err(Outcome::Audit(AuditFailed::Malformed)),
                    Mapped::Control(_) => {}
                    Mapped::Event(event) => {
                        if self.events.send(event).await.is_err() {
                            return Err(Outcome::Clean);
                        }
                    }
                    Mapped::End { usage, finish } => return Ok((usage, finish)),
                }
            }
            let Some(chunk) = pipes.stdout.recv().await else {
                return Err(Outcome::Eof);
            };
            framer.push(&chunk).map_err(Outcome::Stream)?;
        }
    }
}

/// The parts of a launch the answer reads.
struct Reading<'a> {
    pipes: &'a mut Pipes,
    framer: &'a mut LineFramer,
    mapper: &'a mut StreamMapper,
    post: &'a mut PostSeedAudit,
    launch: &'a Launch,
    analytics: Option<bool>,
}

fn claude(key: ErrorKey) -> Finish {
    Finish::Failed(EngineFailure::Claude(key.into()))
}

/// The question's deadline passed before the seed was queued: the launch goes and the attempt
/// reports `Deadline`.
fn expired(launch: &Launch, report: Report) -> Finish {
    launch.lease().revoke();
    report.report(Outcome::Deadline);
    failed(Outcome::Deadline)
}

/// How a round ends on `outcome`.
fn failed(outcome: Outcome) -> Finish {
    match outcome {
        Outcome::Audit(_) => claude(ErrorKey::ClaudeCodeAuditFailed),
        Outcome::SignedOut => claude(ErrorKey::ClaudeCodeSignedOut),
        Outcome::Stream(failure) => Finish::Failed(EngineFailure::ClaudeStream(failure)),
        Outcome::Eof => Finish::Truncated,
        Outcome::Clean | Outcome::Deadline | Outcome::Launch => {
            Finish::Failed(EngineFailure::Provider)
        }
    }
}
