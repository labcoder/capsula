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
    /// Claude Code's own file in Kumochi's owner-only folder (option a).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "owner call 1 picks it; until then only tests sign in"
        )
    )]
    FileWithDacl,
}

/// What the shipped brain allows until the owner answers call 1.
pub(crate) const SIGN_IN: SignInStore = SignInStore::Undecided;

/// The connection's state; the lane does the work.
#[derive(Debug)]
pub(crate) struct Claude {
    lane: Option<Lane>,
    sign_in: SignInStore,
    generation: u64,
    view: ClaudeCodeView,
    /// A discovery is out.
    finding: bool,
    /// Another was asked for while it was.
    again: bool,
}

impl Default for Claude {
    /// No lane: nothing is ever found.
    fn default() -> Self {
        Self::new(None, SIGN_IN, ClaudeCodeView::NotFound)
    }
}

impl Claude {
    fn new(lane: Option<Lane>, sign_in: SignInStore, view: ClaudeCodeView) -> Self {
        Self {
            lane,
            sign_in,
            generation: 0,
            view,
            finding: false,
            again: false,
        }
    }

    /// `[claude_code] enabled = false`: nothing is found or started.
    pub(crate) fn turned_off() -> Self {
        Self::new(None, SIGN_IN, ClaudeCodeView::TurnedOff)
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

    /// Finds the install again, or once the discovery that's out has ended.
    fn find(&mut self) {
        let Some(lane) = &self.lane else {
            return;
        };
        if self.finding {
            self.again = true;
            return;
        }
        self.generation += 1;
        lane.fence(self.generation);
        self.finding = lane.submit(self.generation, Work::Version);
    }

    /// One of Claude Code's ops; every one is answered with the view.
    pub(crate) fn op(&mut self, op: ConnectionOp) {
        // shortcut: Connect, Cancel and Disconnect wait for C2b's sign-in and cleanup; until
        // then nothing can be signed in, so there's nothing to cancel or delete, and under
        // Undecided Connect never starts anything.
        if op == ConnectionOp::FindClaudeCode {
            self.find();
        }
    }

    /// A lane result; true when the view may have changed.
    pub(crate) fn done(&mut self, Done { generation, result }: Done) -> bool {
        if generation != self.generation || !self.finding {
            return false;
        }
        match result {
            Ok(Outcome::Version(_)) => {
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
            Err(failure) => {
                if let Some(view) = failed(failure) {
                    self.view = view;
                }
            }
        }
        self.finding = false;
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
mod tests;
