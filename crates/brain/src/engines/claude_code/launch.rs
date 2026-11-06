//! One Claude Code question on one launch (docs/development/claude-code.md, "A question"). The
//! core admits an [`Attempt`] and its audit task runs the audit before input; the question round
//! that takes it (C5b-1b2) binds the run, asks to send, seeds and reads the answer.
//! Each attempt sends the core exactly one [`Outcome`], through its one [`Report`].

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kumochi_protocol::constants::MCP_SERVER_NAME;
use kumochi_protocol::message::{AuditFailed, ClaudeVersion, ThreadId};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use super::audit::{self, PostSeedAudit, PreInputAudit, Progress};
use super::policy::Reviewed;
use super::stream::{LineFramer, Mapped, StreamMapper};
use super::supervisor::{Launch, LaunchFailed, Pipes, StdinWriter};
use crate::bridge::LaunchId;
use crate::context::{self, Origin};
use crate::engines::ClaudeStreamFailure;

/// How long a warm launch waits for its question [guess, the design's 30 s].
pub(crate) const WARM: Duration = Duration::from_secs(30);
/// How long the audit before input may take once the control lines are queued (C3).
pub(crate) const AUDIT: Duration = Duration::from_secs(3);

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

/// What a launch is fixed to at spawn; equal snapshots may share a launch. Nothing in it is user
/// content.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[expect(dead_code, reason = "C5b-1b2's round takes it")]
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
    #[expect(dead_code, reason = "C5b-1b2's round awaits it")]
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
