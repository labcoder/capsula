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

    #[test]
    fn every_closed_state_and_reason_has_localized_copy() {
        let strings = Strings::english();
        let version = ClaudeVersion {
            major: 2,
            minor: 1,
            patch: 306,
        };
        assert_eq!(
            strings.claude_status(Some(ClaudeCodeView::Finding)),
            "Looking for Claude Code…"
        );
        assert_eq!(
            strings.claude_status(Some(ClaudeCodeView::SignedOut { version })),
            "Claude Code 2.1.306 is signed out."
        );
        assert_eq!(
            strings.claude_status(Some(ClaudeCodeView::Ready {
                version,
                analytics_on: false
            })),
            "Claude Code 2.1.306 is ready."
        );
        for (reason, label) in [
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
        ] {
            assert_eq!(
                strings.claude_status(Some(ClaudeCodeView::AuditFailed(reason))),
                format!("Claude Code’s launch audit failed: {label}.")
            );
        }
        for (view, text) in [
            (None, "Waiting for Claude Code status…"),
            (
                Some(ClaudeCodeView::TurnedOff),
                "Claude Code is turned off in config.",
            ),
            (Some(ClaudeCodeView::NotFound), "Claude Code was not found."),
            (Some(ClaudeCodeView::SigningIn), "Signing in…"),
            (
                Some(ClaudeCodeView::CleanupPending),
                "Cleanup is pending. Choose Look again to retry removing Kumochi’s Claude Code folder.",
            ),
            (
                Some(ClaudeCodeView::InUse),
                "Another Kumochi session is using Claude Code. Close that session, then look again.",
            ),
            (
                Some(ClaudeCodeView::Unavailable),
                "Claude Code is unavailable. Restart Kumochi before trying again.",
            ),
            (
                Some(ClaudeCodeView::Unsupported(Unsupported::Shim)),
                "This Claude Code installation is a wrapper or shim, not the native executable.",
            ),
            (
                Some(ClaudeCodeView::Unsupported(Unsupported::SignInStore)),
                "This build cannot use Claude Code’s sign-in store.",
            ),
            (
                Some(ClaudeCodeView::Unsupported(Unsupported::Unreviewed(
                    version,
                ))),
                "Claude Code 2.1.306 has not been reviewed for this build.",
            ),
            (
                Some(ClaudeCodeView::PolicyFailed(PolicyFailed::ManagedPolicy)),
                "A managed policy is present or could not be ruled out. Claude Code cannot be used.",
            ),
            (
                Some(ClaudeCodeView::PolicyFailed(PolicyFailed::TurnCap)),
                "This Claude Code version’s turn limit is not verified.",
            ),
        ] {
            assert_eq!(strings.claude_status(view), text, "{view:?}");
        }
    }
}
