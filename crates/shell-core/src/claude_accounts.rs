//! Session-fenced Claude Code account controls, without transport or GPUI state.

use kumochi_protocol::message::{ClaudeCodeView, ConnectionOp};

#[derive(Default, Debug)]
pub struct ClaudeAccounts {
    pub generation: Option<u64>,
    pub view: Option<ClaudeCodeView>,
    pub send_failed: bool,
    pub connect_screen: bool,
    waiting: Option<ConnectionOp>,
}

impl ClaudeAccounts {
    pub fn receive(&mut self, current: Option<u64>, generation: u64, view: ClaudeCodeView) -> bool {
        if current != Some(generation) {
            return false;
        }
        self.generation = Some(generation);
        self.view = Some(view);
        self.waiting = None;
        if !matches!(
            view,
            ClaudeCodeView::SignedOut { .. } | ClaudeCodeView::SigningIn
        ) {
            self.connect_screen = false;
        }
        true
    }

    pub fn disconnected(&mut self) {
        *self = Self::default();
    }

    pub fn open_connect(&mut self) {
        if self.waiting.is_none() && matches!(self.view, Some(ClaudeCodeView::SignedOut { .. })) {
            self.connect_screen = true;
        }
    }

    pub fn allows(&self, op: ConnectionOp) -> bool {
        if self.generation.is_none() || self.waiting.is_some() {
            return false;
        }
        match (self.view, op) {
            (Some(ClaudeCodeView::SignedOut { .. }), ConnectionOp::ConnectClaudeCode) => {
                self.connect_screen
            }
            (Some(ClaudeCodeView::SigningIn), ConnectionOp::CancelClaudeSignIn)
            | (Some(ClaudeCodeView::Ready { .. }), ConnectionOp::DisconnectClaudeCode)
            | (
                Some(
                    ClaudeCodeView::CleanupPending
                    | ClaudeCodeView::NotFound
                    | ClaudeCodeView::SignedOut { .. }
                    | ClaudeCodeView::Unsupported(_)
                    | ClaudeCodeView::AuditFailed(_)
                    | ClaudeCodeView::PolicyFailed(_)
                    | ClaudeCodeView::InUse
                    | ClaudeCodeView::Busy,
                ),
                ConnectionOp::FindClaudeCode,
            ) => true,
            _ => false,
        }
    }

    pub fn send(&mut self, op: ConnectionOp, send: impl FnOnce(ConnectionOp) -> bool) -> bool {
        if !self.allows(op) {
            return false;
        }
        if !send(op) {
            self.send_failed = true;
            return false;
        }
        self.send_failed = false;
        self.waiting = Some(op);
        true
    }

    /// Sends `CancelClaudeSignIn` without asking `allows`, for a dismissal that must cancel a
    /// sign-in the brain hasn't answered yet. The brain ignores it when nothing is signing in.
    pub fn cancel_sign_in(&mut self, send: impl FnOnce(ConnectionOp) -> bool) -> bool {
        if !send(ConnectionOp::CancelClaudeSignIn) {
            return false;
        }
        self.waiting = Some(ConnectionOp::CancelClaudeSignIn);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumochi_protocol::message::{AuditFailed, ClaudeVersion, PolicyFailed, Unsupported};

    const VERSION: ClaudeVersion = ClaudeVersion {
        major: 2,
        minor: 1,
        patch: 306,
    };
    const OUT: ClaudeCodeView = ClaudeCodeView::SignedOut { version: VERSION };

    #[test]
    fn connect_requires_the_notice_and_a_settled_discovery() {
        let mut state = ClaudeAccounts::default();
        state.receive(Some(1), 1, OUT);
        assert!(!state.send(ConnectionOp::ConnectClaudeCode, |_| panic!(
            "notice not shown"
        )));
        state.open_connect();
        assert!(state.connect_screen);
        state.receive(Some(1), 1, ClaudeCodeView::Finding);
        assert!(!state.connect_screen);
        assert!(!state.send(ConnectionOp::ConnectClaudeCode, |_| panic!("discovery")));
        state.receive(Some(1), 1, OUT);
        state.open_connect();
        let mut sent = Vec::new();
        assert!(state.send(ConnectionOp::ConnectClaudeCode, |op| {
            sent.push(op);
            true
        }));
        assert!(!state.send(ConnectionOp::ConnectClaudeCode, |_| panic!("duplicate")));
        assert_eq!(sent, [ConnectionOp::ConnectClaudeCode]);
        assert_eq!(state.view, Some(OUT));
    }

    #[test]
    fn each_snapshot_has_only_its_applicable_actions() {
        for (view, allowed) in [
            (ClaudeCodeView::Finding, vec![]),
            (ClaudeCodeView::NotFound, vec![ConnectionOp::FindClaudeCode]),
            (OUT, vec![ConnectionOp::FindClaudeCode]),
            (
                ClaudeCodeView::SigningIn,
                vec![ConnectionOp::CancelClaudeSignIn],
            ),
            (
                ClaudeCodeView::Ready {
                    version: VERSION,
                    analytics_on: true,
                },
                vec![ConnectionOp::DisconnectClaudeCode],
            ),
            (
                ClaudeCodeView::CleanupPending,
                vec![ConnectionOp::FindClaudeCode],
            ),
            (ClaudeCodeView::TurnedOff, vec![]),
            (ClaudeCodeView::Unavailable, vec![]),
            (ClaudeCodeView::Busy, vec![ConnectionOp::FindClaudeCode]),
            (ClaudeCodeView::InUse, vec![ConnectionOp::FindClaudeCode]),
            (
                ClaudeCodeView::Unsupported(Unsupported::Shim),
                vec![ConnectionOp::FindClaudeCode],
            ),
            (
                ClaudeCodeView::AuditFailed(AuditFailed::Malformed),
                vec![ConnectionOp::FindClaudeCode],
            ),
            (
                ClaudeCodeView::PolicyFailed(PolicyFailed::ManagedPolicy),
                vec![ConnectionOp::FindClaudeCode],
            ),
        ] {
            let mut state = ClaudeAccounts::default();
            assert!(state.receive(Some(8), 8, view));
            assert_eq!(state.view, Some(view));
            for op in [
                ConnectionOp::FindClaudeCode,
                ConnectionOp::ConnectClaudeCode,
                ConnectionOp::CancelClaudeSignIn,
                ConnectionOp::DisconnectClaudeCode,
            ] {
                assert_eq!(state.allows(op), allowed.contains(&op), "{view:?} {op:?}");
            }
        }
    }

    #[test]
    fn stale_snapshots_and_failed_sends_do_not_connect() {
        let mut state = ClaudeAccounts::default();
        state.receive(Some(3), 3, OUT);
        state.open_connect();
        assert!(!state.send(ConnectionOp::ConnectClaudeCode, |_| false));
        assert!(state.send_failed);
        assert_eq!(state.view, Some(OUT));
        assert!(!state.receive(Some(3), 2, ClaudeCodeView::SigningIn));
        assert!(state.send_failed);
        state.receive(Some(3), 3, OUT);
        assert!(state.send_failed);
        state.disconnected();
        assert!(!state.receive(None, 3, OUT));
        assert!(!state.allows(ConnectionOp::ConnectClaudeCode));
        state.receive(Some(4), 4, OUT);
        assert!(!state.connect_screen);
    }
}
