//! Claude Code's connection: what discovery found, through the one lane that starts `claude`.
//! A1 owns the Anthropic key's half of the view; every Connections frame is put together from
//! both at the moment it's sent.

use kumochi_protocol::message::{
    ClaudeCodeView, ConnectionOp, ConnectionsView, ControlToShell, Unsupported,
};

use super::{Brain, Full, Link};
use crate::engines::claude_code::install::FindError;
use crate::engines::claude_code::lane::{Done, Failure, Lane, Outcome, Work};

/// Where Claude Code's sign-in may be kept, which owner call 1 decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignInStore {
    /// Not decided: Connect does nothing and a signed-out install shows as unsupported.
    Undecided,
    /// Claude Code's own file in Kumochi's owner-only folder (option a). Only it lets Connect,
    /// Cancel and Disconnect do anything.
    FileWithDacl,
}

/// What the shipped brain allows until the owner answers call 1.
pub(crate) const SIGN_IN: SignInStore = SignInStore::Undecided;

/// The connection's state; the lane does the work.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent facts about the jobs; no two combine into one state"
)]
pub(crate) struct Claude {
    lane: Option<Lane>,
    sign_in: SignInStore,
    /// False under `[claude_code] enabled = false`, where only a marked deletion runs.
    finds: bool,
    generation: u64,
    view: ClaudeCodeView,
    /// A job of this generation is out.
    out: bool,
    /// A Find was asked for while it was.
    again: bool,
    /// A Disconnect is still to finish.
    cleaning: bool,
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
            out: false,
            again: false,
            cleaning: false,
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

    /// Starts `work` under a new generation, which ends whatever an older one still runs.
    fn begin(&mut self, work: Work) {
        let Some(lane) = &self.lane else {
            return;
        };
        self.generation += 1;
        lane.fence(self.generation);
        self.out = lane.submit(self.generation, work);
        self.again = false;
    }

    /// Finds the install again, or retries a deletion still to finish; after the job that's out,
    /// if one is.
    fn find(&mut self) {
        // shortcut: a pending deletion is retried here and at each start, not on a timer; add one
        // if locked files turn out to outlive a session.
        if self.out {
            self.again = true;
            return;
        }
        self.begin(match (self.finds, self.cleaning) {
            (false, _) => Work::Cleanup,
            (true, true) => Work::Disconnect { logout: false },
            (true, false) => Work::Version,
        });
    }

    /// One of Claude Code's ops; every one is answered with the view.
    pub(crate) fn op(&mut self, op: ConnectionOp) {
        let signs_in = self.sign_in == SignInStore::FileWithDacl;
        match op {
            ConnectionOp::FindClaudeCode => self.find(),
            ConnectionOp::ConnectClaudeCode
                if signs_in
                    && !self.out
                    && matches!(self.view, ClaudeCodeView::SignedOut { .. }) =>
            {
                self.view = ClaudeCodeView::SigningIn;
                self.begin(Work::Login);
            }
            // The new generation ends the sign-in; the Status after it shows where it got.
            ConnectionOp::CancelClaudeSignIn
                if signs_in && self.view == ClaudeCodeView::SigningIn =>
            {
                self.begin(Work::Version);
            }
            ConnectionOp::DisconnectClaudeCode if signs_in && self.finds && !self.cleaning => {
                self.cleaning = true;
                self.view = ClaudeCodeView::CleanupPending;
                self.begin(Work::Disconnect { logout: true });
            }
            _ => {}
        }
    }

    /// A lane result; true when the view may have changed.
    pub(crate) fn done(&mut self, Done { generation, result }: Done) -> bool {
        if generation != self.generation || !self.out {
            return false;
        }
        match result {
            Ok(Outcome::Version(_) | Outcome::LoginEnded) => {
                let lane = self.lane.as_ref().expect("a result comes from the lane");
                if lane.submit(self.generation, Work::Status) {
                    return false;
                }
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
            Err(failure) => {
                if let Some(view) = failed(failure) {
                    self.view = view;
                }
            }
        }
        self.out = false;
        if std::mem::take(&mut self.again) {
            self.find();
        }
        true
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

/// The view a failure leaves; `None` keeps the last one, for a call that never started.
fn failed(failure: Failure) -> Option<ClaudeCodeView> {
    use kumochi_protocol::message::AuditFailed;

    Some(match failure {
        Failure::Refused => return None,
        Failure::Busy => ClaudeCodeView::InUse,
        Failure::Cleanup => ClaudeCodeView::CleanupPending,
        Failure::Find(FindError::NotFound) => ClaudeCodeView::NotFound,
        Failure::Find(FindError::Shim) => ClaudeCodeView::Unsupported(Unsupported::Shim),
        Failure::Unsupported(unsupported) => ClaudeCodeView::Unsupported(unsupported),
        Failure::Policy(policy) => ClaudeCodeView::PolicyFailed(policy),
        Failure::Deadline => ClaudeCodeView::AuditFailed(AuditFailed::Deadline),
        Failure::Version => ClaudeCodeView::AuditFailed(AuditFailed::Version),
        Failure::Folders
        | Failure::Spawn
        | Failure::TooLong
        | Failure::Pipe
        | Failure::Status(_) => ClaudeCodeView::AuditFailed(AuditFailed::Malformed),
    })
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
pub(super) mod tests;
