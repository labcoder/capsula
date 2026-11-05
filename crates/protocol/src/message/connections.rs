//! The Anthropic key and Claude Code connections: what the shell asks of them and what it shows.
//! Nothing here carries a key, a token or anything Claude Code printed.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Which connection a model choice runs on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectionKind {
    /// The `ChatGPT` account; every choice saved before Claude existed.
    #[default]
    ChatGpt,
    /// The user's Anthropic API key.
    AnthropicKey,
    /// The user's installed Claude Code.
    ClaudeCode,
}

/// Something the user asked of a Claude connection; answered by one
/// [`crate::message::ControlToShell::Connections`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionOp {
    /// The shell staged a pasted key in Credential Manager; the brain checks it and moves it.
    AnthropicKeyStaged,
    /// Check the saved key again, after it was left unconfirmed.
    CheckAnthropicKey,
    /// Forget the saved key.
    RemoveAnthropicKey,
    /// Start Claude Code's own sign-in, in a console the user answers.
    ConnectClaudeCode,
    /// Stop that sign-in.
    CancelClaudeSignIn,
    /// Sign Claude Code out and delete Kumochi's folder for it.
    DisconnectClaudeCode,
    /// Look for the installed Claude Code again.
    FindClaudeCode,
}

/// Both Claude connections, authoritative: the shell never moves on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsView {
    /// The Anthropic key.
    pub anthropic: KeyView,
    /// Claude Code.
    pub claude_code: ClaudeCodeView,
}

/// Where the Anthropic key stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyView {
    /// No key saved.
    Missing,
    /// A key is being checked.
    Checking,
    /// The key works.
    Ready,
    /// The key was refused, and why.
    Rejected(KeyRejection),
    /// The key is kept, but Anthropic couldn't be reached to confirm it.
    Unconfirmed,
}

/// Why a key was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyRejection {
    /// It isn't shaped like an Anthropic key.
    Format,
    /// Anthropic refused it.
    Refused,
    /// Anthropic limited its rate.
    RateLimited,
    /// Its account is out of credit.
    Credit,
}

/// Where Claude Code stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeCodeView {
    /// `[claude_code] enabled = false`.
    TurnedOff,
    /// No install was found.
    NotFound,
    /// An install was found that Kumochi won't use.
    Unsupported(Unsupported),
    /// Found and not signed in.
    SignedOut {
        /// Its version.
        version: ClaudeVersion,
    },
    /// Claude Code's own sign-in is running.
    SigningIn,
    /// Signed in and usable.
    Ready {
        /// Its version.
        version: ClaudeVersion,
        /// Whether Claude Code reported its analytics as on.
        analytics_on: bool,
    },
    /// The last launch failed its audit.
    AuditFailed(AuditFailed),
    /// A policy check closed the route.
    PolicyFailed(PolicyFailed),
    /// A Disconnect's cleanup is still to finish.
    CleanupPending,
}

/// Why a found Claude Code isn't used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unsupported {
    /// It's a shim or wrapper, not the native executable.
    Shim,
    /// Its sign-in store isn't one Kumochi accepts.
    SignInStore,
    /// Kumochi hasn't reviewed this version.
    Unreviewed(ClaudeVersion),
}

/// Which audit check failed: never what it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditFailed {
    /// The version.
    Version,
    /// The slash commands.
    Commands,
    /// The agents.
    Agents,
    /// The MCP server.
    McpServer,
    /// The tools.
    Tools,
    /// The plugins.
    Plugins,
    /// The skills.
    Skills,
    /// Where the API key came from.
    KeySource,
    /// Where the sign-in token came from.
    TokenSource,
    /// The facts didn't arrive in time.
    Deadline,
    /// The facts couldn't be read.
    Malformed,
}

/// Which policy closed the route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyFailed {
    /// A managed policy is present on this machine, or couldn't be ruled out.
    ManagedPolicy,
    /// This version's turn cap isn't verified, or didn't hold.
    TurnCap,
}

/// A Claude Code version: three numbers, as `claude --version` prints them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClaudeVersion {
    /// The major number.
    pub major: u16,
    /// The minor number.
    pub minor: u16,
    /// The patch number.
    pub patch: u16,
}

/// A version wasn't three dot-separated numbers that fit in 16 bits each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a Claude Code version is three dot-separated numbers of 0–65535")]
pub struct VersionError;

impl FromStr for ClaudeVersion {
    type Err = VersionError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = text.split('.').map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(VersionError);
            }
            part.parse::<u16>().map_err(|_| VersionError)
        });
        let mut next = || parts.next().ok_or(VersionError)?;
        let version = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        if parts.next().is_some() {
            return Err(VersionError);
        }
        Ok(version)
    }
}

impl fmt::Display for ClaudeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_three_numbers_of_sixteen_bits() {
        let ok = |text: &str| text.parse::<ClaudeVersion>();
        assert_eq!(
            ok("0.0.0"),
            Ok(ClaudeVersion {
                major: 0,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            ok("65535.65535.65535"),
            Ok(ClaudeVersion {
                major: 65535,
                minor: 65535,
                patch: 65535
            })
        );
        assert_eq!(
            ok("2.1.306").map(|v| v.to_string()),
            Ok("2.1.306".to_owned())
        );
        for bad in [
            "",
            "2.1",
            "2.1.306.1",
            "2..306",
            "2.1.",
            "+2.1.306",
            "2.1.-1",
            "2.1.65536",
            " 2.1.306",
        ] {
            assert_eq!(ok(bad), Err(VersionError), "{bad:?}");
        }
    }
}
