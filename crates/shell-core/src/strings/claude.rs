//! Closed Claude Code state and reason labels.

use fluent_bundle::FluentArgs;
use kumochi_protocol::message::{AuditFailed, ClaudeCodeView, PolicyFailed, Unsupported};

use super::Strings;

impl Strings {
    pub fn claude_status(&self, view: Option<ClaudeCodeView>) -> String {
        let mut args = FluentArgs::new();
        let id = match view {
            None => "claude-status-waiting",
            Some(ClaudeCodeView::Finding) => "claude-status-finding",
            Some(ClaudeCodeView::NotFound) => "claude-status-not-found",
            Some(ClaudeCodeView::TurnedOff) => "claude-status-off",
            Some(ClaudeCodeView::SigningIn) => "claude-status-signing",
            Some(ClaudeCodeView::CleanupPending) => "claude-status-cleanup",
            Some(ClaudeCodeView::InUse) => "claude-status-in-use",
            Some(ClaudeCodeView::Unavailable) => "claude-status-unavailable",
            Some(ClaudeCodeView::Busy) => "error-claude-code-busy",
            Some(
                view @ (ClaudeCodeView::SignedOut { version }
                | ClaudeCodeView::Ready { version, .. }),
            ) => {
                args.set("version", version.to_string());
                if matches!(view, ClaudeCodeView::SignedOut { .. }) {
                    "claude-status-out"
                } else {
                    "claude-status-ready"
                }
            }
            Some(ClaudeCodeView::Unsupported(reason)) => {
                let reason = match reason {
                    Unsupported::Shim => "shim",
                    Unsupported::SignInStore => "store",
                    Unsupported::Unreviewed(version) => {
                        args.set("version", version.to_string());
                        "version"
                    }
                };
                args.set("reason", reason);
                "claude-status-unsupported"
            }
            Some(ClaudeCodeView::AuditFailed(reason)) => {
                args.set(
                    "reason",
                    match reason {
                        AuditFailed::Version => "version",
                        AuditFailed::Commands => "commands",
                        AuditFailed::Agents => "agents",
                        AuditFailed::McpServer => "mcp",
                        AuditFailed::Tools => "tools",
                        AuditFailed::Plugins => "plugins",
                        AuditFailed::Skills => "skills",
                        AuditFailed::KeySource => "key",
                        AuditFailed::TokenSource => "token",
                        AuditFailed::Deadline => "deadline",
                        AuditFailed::Malformed => "malformed",
                    },
                );
                "claude-status-audit"
            }
            Some(ClaudeCodeView::PolicyFailed(reason)) => {
                args.set(
                    "reason",
                    match reason {
                        PolicyFailed::ManagedPolicy => "managed",
                        PolicyFailed::TurnCap => "turns",
                    },
                );
                "claude-status-policy"
            }
        };
        self.format(id, Some(&args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumochi_protocol::message::ClaudeVersion;

    const AUDIT_LABELS: [(AuditFailed, &str); 11] = [
        (AuditFailed::Version, "version mismatch"),
        (AuditFailed::Commands, "unexpected slash commands"),
        (AuditFailed::Agents, "unexpected agents"),
        (AuditFailed::McpServer, "unexpected MCP server"),
        (AuditFailed::Tools, "unexpected tools"),
        (AuditFailed::Plugins, "unexpected plugins"),
        (AuditFailed::Skills, "unexpected skills"),
        (AuditFailed::KeySource, "unexpected API key source"),
        (AuditFailed::TokenSource, "unexpected sign-in source"),
        (AuditFailed::Deadline, "audit timed out"),
        (AuditFailed::Malformed, "unreadable launch facts"),
    ];

    #[test]
    fn every_closed_state_and_reason_has_localized_copy() {
        let strings = Strings::english();
        let version = ClaudeVersion {
            major: 2,
            minor: 1,
            patch: 306,
        };
        let mut copies = std::collections::HashSet::new();
        let mut version_args = FluentArgs::new();
        version_args.set("version", "2.1.306");
        for (state, id) in [
            (ClaudeCodeView::SignedOut { version }, "claude-status-out"),
            (
                ClaudeCodeView::Ready {
                    version,
                    analytics_on: false,
                },
                "claude-status-ready",
            ),
        ] {
            let copy = strings.claude_status(Some(state));
            assert_eq!(copy, strings.format(id, Some(&version_args)), "{state:?}");
            assert!(copy.contains("2.1.306"), "{state:?}");
            assert!(copies.insert(copy), "{state:?}");
        }
        version_args.set("reason", "version");
        let unreviewed = strings.claude_status(Some(ClaudeCodeView::Unsupported(
            Unsupported::Unreviewed(version),
        )));
        assert_eq!(
            unreviewed,
            strings.format("claude-status-unsupported", Some(&version_args))
        );
        assert!(unreviewed.contains("2.1.306"));
        assert!(copies.insert(unreviewed));
        for (reason, label) in AUDIT_LABELS {
            let copy = strings.claude_status(Some(ClaudeCodeView::AuditFailed(reason)));
            assert!(copy.contains(label), "{reason:?}");
            assert!(copies.insert(copy), "{reason:?}");
        }
        let cleanup = strings.claude_status(Some(ClaudeCodeView::CleanupPending));
        assert!(
            cleanup.contains("Look again")
                && cleanup.contains("removing Kumochi’s Claude Code folder")
        );
        assert!(copies.insert(cleanup));
        let policy = strings.claude_status(Some(ClaudeCodeView::PolicyFailed(
            PolicyFailed::ManagedPolicy,
        )));
        assert!(policy.contains("managed policy") && policy.contains("Claude Code cannot be used"));
        assert!(copies.insert(policy));
        for (view, id, reason) in [
            (None, "claude-status-waiting", None),
            (Some(ClaudeCodeView::Finding), "claude-status-finding", None),
            (Some(ClaudeCodeView::TurnedOff), "claude-status-off", None),
            (
                Some(ClaudeCodeView::NotFound),
                "claude-status-not-found",
                None,
            ),
            (
                Some(ClaudeCodeView::SigningIn),
                "claude-status-signing",
                None,
            ),
            (Some(ClaudeCodeView::InUse), "claude-status-in-use", None),
            (
                Some(ClaudeCodeView::Unavailable),
                "claude-status-unavailable",
                None,
            ),
            (Some(ClaudeCodeView::Busy), "error-claude-code-busy", None),
            (
                Some(ClaudeCodeView::Unsupported(Unsupported::Shim)),
                "claude-status-unsupported",
                Some("shim"),
            ),
            (
                Some(ClaudeCodeView::Unsupported(Unsupported::SignInStore)),
                "claude-status-unsupported",
                Some("store"),
            ),
            (
                Some(ClaudeCodeView::PolicyFailed(PolicyFailed::TurnCap)),
                "claude-status-policy",
                Some("turns"),
            ),
        ] {
            let args = reason.map(|reason| {
                let mut args = FluentArgs::new();
                args.set("reason", reason);
                args
            });
            let copy = strings.claude_status(view);
            assert_eq!(copy, strings.format(id, args.as_ref()), "{view:?}");
            assert!(!copy.is_empty() && copy != id, "{view:?}");
            assert!(copies.insert(copy), "{view:?}");
        }
    }
}
