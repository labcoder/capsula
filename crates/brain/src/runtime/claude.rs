//! Claude Code's connection: what discovery found, through the one lane that starts `claude`.
//! A1 owns the Anthropic key's half of the view; every Connections frame is put together from
//! both at the moment it's sent.

use std::sync::Arc;

use kumochi_protocol::message::{
    AuditFailed, ClaudeCodeView, ClaudeVersion, ConnectionOp, ConnectionsView, ControlToShell,
    ThreadId, Unsupported,
};

use super::{Brain, Full, Link};
use crate::coordinator::StoreOp;
use crate::engines::claude_code::install::FindError;
use crate::engines::claude_code::lane::{Done, Failure, Lane, Outcome, Work};
use crate::engines::claude_code::supervisor::{
    self, Control, Launch, LaunchRefused, MAX_LAUNCH, Question,
};

/// Where Claude Code's sign-in may be kept, which owner call 1 decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignInStore {
    /// Not decided: Connect does nothing and a signed-out install shows as unsupported.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the off position, kept under test")
    )]
    Undecided,
    /// Claude Code's own file in Kumochi's owner-only folder (option a). Only it lets Connect,
    /// Cancel and Disconnect do anything.
    FileWithDacl,
}

/// What the shipped brain allows: the owner chose option a for call 1 (2026-11-05).
pub(crate) const SIGN_IN: SignInStore = SignInStore::FileWithDacl;

/// The connection's state; the lane does the work.
#[derive(Debug)]
pub(crate) struct Claude {
    lane: Option<Lane>,
    sign_in: SignInStore,
    /// False under `[claude_code] enabled = false`, where only a marked deletion runs.
    finds: bool,
    generation: u64,
    view: ClaudeCodeView,
    /// The job of this generation that's out, if one is.
    out: Option<Work>,
    /// A Find was asked for while it was.
    again: bool,
    /// A Disconnect is still to finish.
    cleaning: bool,
    /// The launches admitted, by thread, until each one's teardown is over.
    launches: Vec<(ThreadId, Arc<Control>)>,
    /// The version whose audit failed in this brain, until a different version is found once
    /// its marker is written.
    quarantine: Option<Quarantine>,
}

/// A version this brain saw fail its audit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Quarantine {
    version: ClaudeVersion,
    reason: AuditFailed,
    /// Its marker was written.
    durable: bool,
}

impl Default for Claude {
    /// No lane: nothing is ever found.
    fn default() -> Self {
        Self::new(None, SIGN_IN, ClaudeCodeView::NotFound)
    }
}

impl Claude {
    pub(super) fn new(lane: Option<Lane>, sign_in: SignInStore, view: ClaudeCodeView) -> Self {
        Self {
            lane,
            sign_in,
            finds: true,
            generation: 0,
            view,
            out: None,
            again: false,
            cleaning: false,
            launches: Vec::new(),
            quarantine: None,
        }
    }

    /// `[claude_code] enabled = false`: nothing is found or started, and `lane` only finishes a
    /// deletion an earlier Disconnect marked.
    pub(crate) fn turned_off(lane: Option<Lane>) -> Self {
        let mut claude = Self::new(lane, SIGN_IN, ClaudeCodeView::TurnedOff);
        claude.finds = false;
        claude.find();
        claude
    }

    /// Discovery through `lane`, starting now.
    pub(crate) fn start(lane: Lane, sign_in: SignInStore) -> Self {
        let mut claude = Self::new(Some(lane), sign_in, ClaudeCodeView::NotFound);
        claude.find();
        claude
    }

    pub(crate) fn view(&self) -> ClaudeCodeView {
        self.view
    }

    /// Starts `work` under a new generation, which ends whatever an older one still runs; false
    /// when it isn't out.
    fn begin(&mut self, work: Work) -> bool {
        let Some(lane) = &self.lane else {
            return false;
        };
        self.generation += 1;
        lane.fence(self.generation);
        self.again = false;
        self.submit(work)
    }

    /// Queues `work` under this generation. When the lane can't take it, nothing is out and the
    /// view says so, since no result will come to settle it.
    fn submit(&mut self, work: Work) -> bool {
        let admitted = self
            .lane
            .as_ref()
            .is_some_and(|lane| lane.submit(self.generation, work));
        self.out = admitted.then_some(work);
        if !admitted {
            self.view = ClaudeCodeView::Unavailable;
            self.revoke_all();
        }
        admitted
    }

    /// Finds the install again, or retries a deletion still to finish; after the job that's out,
    /// if one is.
    fn find(&mut self) {
        // shortcut: a pending deletion is retried here and at each start, not on a timer; add one
        // if locked files turn out to outlive a session.
        if self.out.is_some() {
            self.again = true;
            return;
        }
        self.begin(match (self.finds, self.cleaning) {
            (false, _) => Work::Cleanup,
            (true, true) => Work::Disconnect { logout: false },
            (true, false) => match self.quarantine {
                Some(Quarantine {
                    version,
                    durable: false,
                    ..
                }) => Work::Quarantine(version),
                _ => Work::Version,
            },
        });
    }

    /// Refuses `version` from now on for `reason`: every launch ends, and the lane writes its
    /// marker, which survives Disconnect and restarts. Until that write succeeds, every version
    /// stays refused. The first failure's reason stays.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "C5b-1b's question is the first caller")
    )]
    pub(crate) fn audit_failed(&mut self, version: ClaudeVersion, reason: AuditFailed) {
        if self.quarantine.is_none_or(|held| held.version != version) {
            self.quarantine = Some(Quarantine {
                version,
                reason,
                durable: false,
            });
        }
        self.revoke_all();
        if let Some(held) = self.quarantine {
            self.view = ClaudeCodeView::AuditFailed(held.reason);
        }
        self.find();
    }

    /// One of Claude Code's ops; every one is answered with the view.
    pub(crate) fn op(&mut self, op: ConnectionOp) {
        let signs_in = self.sign_in == SignInStore::FileWithDacl;
        match op {
            ConnectionOp::FindClaudeCode => self.find(),
            ConnectionOp::ConnectClaudeCode
                if signs_in
                    && self.out.is_none()
                    && matches!(self.view, ClaudeCodeView::SignedOut { .. }) =>
            {
                if self.begin(Work::Login) {
                    self.view = ClaudeCodeView::SigningIn;
                }
            }
            // The new generation ends the sign-in; the Status after it shows where it got. Only
            // the sign-in itself is cancelled, so a repeated Cancel queues nothing more.
            ConnectionOp::CancelClaudeSignIn
                if signs_in
                    && self.view == ClaudeCodeView::SigningIn
                    && self.out == Some(Work::Login) =>
            {
                self.begin(Work::Version);
            }
            // A Disconnect the lane can't take leaves nothing marked, so it may be asked again.
            ConnectionOp::DisconnectClaudeCode if signs_in && self.finds && !self.cleaning => {
                // Before the Disconnect is queued, so the lane ends every launch before it signs
                // out.
                self.revoke_all();
                self.cleaning = self.begin(Work::Disconnect { logout: true });
                if self.cleaning {
                    self.view = ClaudeCodeView::CleanupPending;
                }
            }
            _ => {}
        }
    }

    /// A lane result; true when the view may have changed.
    pub(crate) fn done(&mut self, Done { generation, result }: Done) -> bool {
        if generation != self.generation || self.out.is_none() {
            return false;
        }
        if let Some(held) = &mut self.quarantine
            && self.out == Some(Work::Quarantine(held.version))
            && !matches!(
                result,
                Err(Failure::Quarantine | Failure::Refused | Failure::Busy)
            )
        {
            held.durable = true;
        }
        // A version found past the quarantine check: the lane cleared every other's marker.
        if let Ok(Outcome::Version(found))
        | Err(Failure::Unsupported(Unsupported::Unreviewed(found))) = result
            && self
                .quarantine
                .is_some_and(|held| held.durable && held.version != found)
        {
            self.quarantine = None;
        }
        match result {
            Ok(Outcome::Version(_) | Outcome::LoginEnded) => {
                if self.submit(Work::Status) {
                    return false;
                }
            }
            Ok(Outcome::Status { .. }) if let Some(held) = self.quarantine => {
                self.view = ClaudeCodeView::AuditFailed(held.reason);
            }
            Ok(Outcome::Status { version, signed_in }) => {
                self.view = match (signed_in, self.sign_in) {
                    (true, _) => ClaudeCodeView::Ready {
                        version,
                        // The audit's to report, when C5b runs it.
                        analytics_on: false,
                    },
                    (false, SignInStore::Undecided) => {
                        ClaudeCodeView::Unsupported(Unsupported::SignInStore)
                    }
                    (false, SignInStore::FileWithDacl) => ClaudeCodeView::SignedOut { version },
                };
            }
            Ok(Outcome::Removed) => {
                self.cleaning = false;
                if self.finds {
                    self.again = true;
                } else {
                    self.view = ClaudeCodeView::TurnedOff;
                }
            }
            Err(Failure::Quarantined(version)) => {
                self.view = ClaudeCodeView::AuditFailed(
                    self.quarantine
                        .filter(|held| held.version == version)
                        .map_or(AuditFailed::Version, |held| held.reason),
                );
            }
            Err(failure) => {
                self.view = match self.quarantine {
                    Some(held) if !held.durable => ClaudeCodeView::AuditFailed(held.reason),
                    _ => failed(failure),
                };
            }
        }
        self.out = None;
        if std::mem::take(&mut self.again) {
            self.find();
        }
        // A settled view that isn't Ready ends every launch; one still being found doesn't.
        if self.out.is_none() && !matches!(self.view, ClaudeCodeView::Ready { .. }) {
            self.revoke_all();
        }
        true
    }

    /// Queues a question launch behind whatever the lane is doing. Its deadline is fixed now.
    ///
    /// # Errors
    /// [`LaunchRefused::NotReady`] unless Claude Code is Ready under the decided sign-in store,
    /// on, no Disconnect is pending and no version's audit failed; [`LaunchRefused::Lifetime`]
    /// over [`MAX_LAUNCH`]; [`LaunchRefused::Identity`] when no launch id can be drawn;
    /// [`LaunchRefused::Busy`] when the lane's queue is full. Nothing is queued then.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "C5b's engine is the first caller")
    )]
    pub(crate) fn launch(&mut self, question: Question) -> Result<Launch, LaunchRefused> {
        self.launches.retain(|(_, control)| !control.is_ended());
        let ready = matches!(self.view, ClaudeCodeView::Ready { .. })
            && self.sign_in == SignInStore::FileWithDacl
            && self.finds
            && !self.cleaning
            && self.quarantine.is_none();
        let lane = self
            .lane
            .as_ref()
            .filter(|_| ready)
            .ok_or(LaunchRefused::NotReady)?;
        if question.lifetime > MAX_LAUNCH {
            return Err(LaunchRefused::Lifetime);
        }
        let thread = question.thread;
        let (start, launch) = supervisor::admit(question)?;
        let control = start.control();
        if !lane.launch(start) {
            return Err(LaunchRefused::Busy);
        }
        self.launches.push((thread, control));
        Ok(launch)
    }

    /// Revokes every launch; each one's teardown runs on the lane before its next call.
    fn revoke_all(&mut self) {
        for (_, control) in self.launches.drain(..) {
            control.withdraw();
        }
    }

    /// Revokes a thread's launches as `op` deletes it, before the store sees it.
    pub(crate) fn deleting(&mut self, op: &StoreOp) {
        if let StoreOp::Delete { thread } = op {
            self.revoke_thread(*thread);
        }
    }

    /// Revokes `thread`'s launches.
    fn revoke_thread(&mut self, thread: ThreadId) {
        self.launches.retain(|(held, control)| {
            if *held != thread {
                return true;
            }
            control.withdraw();
            false
        });
    }

    pub(crate) async fn next(&mut self) -> Done {
        match &mut self.lane {
            Some(lane) => lane.next().await,
            None => std::future::pending().await,
        }
    }

    /// Ends the lane by `deadline`; true when nothing it started can still be running.
    pub(crate) async fn close(&mut self, deadline: tokio::time::Instant) -> bool {
        match &mut self.lane {
            Some(lane) => lane.close(deadline).await,
            None => true,
        }
    }
}

/// The view a failure leaves.
fn failed(failure: Failure) -> ClaudeCodeView {
    match failure {
        // The lane is stuck or closing: whatever showed before can't be acted on any more.
        Failure::Refused => ClaudeCodeView::Unavailable,
        Failure::Busy => ClaudeCodeView::InUse,
        Failure::Cleanup => ClaudeCodeView::CleanupPending,
        Failure::Find(FindError::NotFound) => ClaudeCodeView::NotFound,
        Failure::Find(FindError::Shim) => ClaudeCodeView::Unsupported(Unsupported::Shim),
        Failure::Unsupported(unsupported) => ClaudeCodeView::Unsupported(unsupported),
        Failure::Policy(policy) => ClaudeCodeView::PolicyFailed(policy),
        Failure::Deadline => ClaudeCodeView::AuditFailed(AuditFailed::Deadline),
        Failure::Version | Failure::Quarantined(_) => {
            ClaudeCodeView::AuditFailed(AuditFailed::Version)
        }
        Failure::Quarantine => ClaudeCodeView::AuditFailed(AuditFailed::Malformed),
        Failure::Folders
        | Failure::Spawn
        | Failure::TooLong
        | Failure::Pipe
        | Failure::Status(_) => ClaudeCodeView::AuditFailed(AuditFailed::Malformed),
    }
}

impl Brain {
    /// Both connections as they stand now.
    pub(super) fn connections_frame(&self) -> ControlToShell {
        ControlToShell::Connections(ConnectionsView {
            anthropic: self.account.connections.anthropic(),
            claude_code: self.claude.view(),
        })
    }

    /// A frame for the shell, with any Connections frame put together from both owners now.
    pub(super) fn composed(&self, frame: &ControlToShell) -> ControlToShell {
        match frame {
            ControlToShell::Connections(_) => self.connections_frame(),
            other => other.clone(),
        }
    }

    pub(super) fn claude_op(
        &mut self,
        op: ConnectionOp,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        self.claude.op(op);
        match link {
            Some(link) => link.control(&self.connections_frame()),
            None => Ok(()),
        }
    }

    pub(super) fn claude_done(
        &mut self,
        done: Done,
        link: Option<&mut Link<'_, '_>>,
    ) -> Result<(), Full> {
        let changed = self.claude.done(done);
        match link {
            Some(link) if changed => link.control(&self.connections_frame()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod quarantine_tests;
#[cfg(test)]
pub(super) mod tests;
