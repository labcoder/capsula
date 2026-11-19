//! The Anthropic key and Claude Code connections: what the shell asks of them and what it shows.
//! Nothing here carries a key, a token or anything Claude Code printed.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Accepted trimmed API-key byte lengths for validation and staging.
pub const ANTHROPIC_KEY_BYTES: std::ops::RangeInclusive<usize> = 16..=2048;

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
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionOp {
    /// The shell staged a pasted key in Credential Manager; the brain checks it and moves it.
    AnthropicKeyStaged {
        /// The brain-issued one-use staging identity.
        plan: KeyStageId,
    },
    /// Check the saved key again, after it was left unconfirmed.
    CheckAnthropicKey,
    /// Forget the exact saved key that the user confirmed.
    RemoveAnthropicKey {
        /// The confirmed authority revision, unrelated to the key material.
        revision: u64,
        /// The shell's outstanding removal request number.
        request: u64,
    },
    /// Start Claude Code's own sign-in, in a console the user answers.
    ConnectClaudeCode,
    /// Stop that sign-in.
    CancelClaudeSignIn,
    /// Sign Claude Code out and delete Kumochi's folder for it.
    DisconnectClaudeCode,
    /// Look for the installed Claude Code again.
    FindClaudeCode,
    /// Ask the brain for names for a validated-length paste, never its bytes.
    BeginAnthropicKeyStage {
        /// The shell's outstanding request number.
        request: u64,
        /// The validated trimmed byte length, not key text.
        len: u16,
    },
    /// Recheck the saved key after a paste whose outcome is unknown.
    ReconcileAnthropicKey {
        /// The shell's outstanding recovery check number.
        request: u64,
    },
}

impl fmt::Debug for ConnectionOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnthropicKeyStaged { plan } => f
                .debug_struct("AnthropicKeyStaged")
                .field("plan", plan)
                .finish(),
            Self::CheckAnthropicKey => f.write_str("CheckAnthropicKey"),
            Self::RemoveAnthropicKey { .. } => f.write_str("RemoveAnthropicKey"),
            Self::ConnectClaudeCode => f.write_str("ConnectClaudeCode"),
            Self::CancelClaudeSignIn => f.write_str("CancelClaudeSignIn"),
            Self::DisconnectClaudeCode => f.write_str("DisconnectClaudeCode"),
            Self::FindClaudeCode => f.write_str("FindClaudeCode"),
            Self::BeginAnthropicKeyStage { .. } => f.write_str("BeginAnthropicKeyStage"),
            Self::ReconcileAnthropicKey { .. } => f.write_str("ReconcileAnthropicKey"),
        }
    }
}

/// A brain-minted write-once staging identity; never key material.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyStageId(pub [u8; 16]);

impl fmt::Debug for KeyStageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyStageId(***)")
    }
}

/// Bounded metadata for the shell's one-use writer.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyStagePlan {
    /// The generation whose names may be written once.
    pub id: KeyStageId,
    /// The trimmed validated key length.
    pub len: u16,
}

impl fmt::Debug for KeyStagePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyStagePlan(***)")
    }
}

impl KeyStagePlan {
    /// Refuses lengths that cannot represent a validated API-key paste.
    pub fn layout(self) -> Option<crate::constants::CredentialLayout> {
        if !ANTHROPIC_KEY_BYTES.contains(&usize::from(self.len)) {
            return None;
        }
        crate::constants::CredentialLayout::new(usize::from(self.len))
    }
}

/// A content-free paste outcome, separate from the saved key's view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyStageFailure {
    /// A credential or authority operation failed.
    Unavailable,
    /// The permit was consumed, superseded, disconnected or removed.
    Changed,
    /// The supplied length cannot describe an API key.
    InvalidLength,
    /// Present, complete bytes were not an accepted API key.
    Rejected(KeyRejection),
}

/// Why a specific key removal didn't commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyRemovalFailure {
    /// The confirmed key no longer matches the authority head.
    Changed,
    /// The worker couldn't commit the removal; the same revision can be retried.
    Unconfirmed,
}

/// Both Claude connections, authoritative: the shell never moves on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsView {
    /// The Anthropic key.
    pub anthropic: KeyView,
    /// The authority revision for the displayed saved key, when known.
    pub anthropic_revision: Option<u64>,
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
    /// A Claude subscription token isn't an API key.
    SubscriptionToken,
    /// An admin key can't be used for inference.
    AdminKey,
    /// The key needs a workspace header that this connection doesn't accept.
    WorkspaceRequired,
}

/// Validate a pasted Anthropic API key, borrowing its trimmed text without copying it.
///
/// # Errors
/// Returns a closed rejection for subscription/admin tokens or malformed key text.
pub fn validate_anthropic_key(text: &str) -> Result<&str, KeyRejection> {
    let key = text.trim();
    if key.starts_with("sk-ant-oat") {
        return Err(KeyRejection::SubscriptionToken);
    }
    if key.starts_with("sk-ant-admin") {
        return Err(KeyRejection::AdminKey);
    }
    if !ANTHROPIC_KEY_BYTES.contains(&key.len())
        || !key.bytes().all(|byte| byte.is_ascii_graphic())
        || !["sk-ant-api", "sk-ant-usr-", "sk-ant-svc-"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
    {
        return Err(KeyRejection::Format);
    }
    Ok(key)
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
    /// Another Kumochi holds Claude Code, or this one doesn't hold its own accounts yet; nothing
    /// was started.
    InUse,
    /// Claude Code can't take work: a call's processes weren't confirmed ended, so nothing starts
    /// until Kumochi restarts.
    Unavailable,
    /// An admitted discovery round is checking the install and sign-in.
    Finding,
    /// The lane's queue was too full to take the last call; it drains by itself.
    Busy,
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
    fn staging_frames_refuse_truncated_missing_trailing_and_oversized_payloads() {
        use crate::frame::{self, FrameError, Pipe};
        use crate::message::ControlToBrain;
        for op in [
            ConnectionOp::BeginAnthropicKeyStage {
                request: 1,
                len: 16,
            },
            ConnectionOp::AnthropicKeyStaged {
                plan: KeyStageId([1; 16]),
            },
        ] {
            let message = ControlToBrain::Connection(op);
            let encoded = frame::encode(&message, Pipe::Control).expect("encoded");
            assert_eq!(
                frame::read::<ControlToBrain>(&mut encoded.as_slice(), Pipe::Control)
                    .expect("complete"),
                Some(message)
            );
            for end in 1..encoded.len() {
                assert!(matches!(
                    frame::read::<ControlToBrain>(&mut &encoded[..end], Pipe::Control),
                    Err(FrameError::Truncated)
                ));
            }
            let payload = &encoded[4..];
            assert!(matches!(
                frame::decode::<ControlToBrain>(&payload[..payload.len() - 1]),
                Err(FrameError::Malformed(
                    postcard::Error::DeserializeUnexpectedEnd
                ))
            ));
            let mut trailing = payload.to_vec();
            trailing.push(0);
            assert!(matches!(
                frame::decode::<ControlToBrain>(&trailing),
                Err(FrameError::Malformed(
                    postcard::Error::DeserializeBadEncoding
                ))
            ));
        }
        assert!(matches!(
            frame::read::<ControlToBrain>(&mut &[0, 0, 0, 0][..], Pipe::Control),
            Err(FrameError::Malformed(
                postcard::Error::DeserializeUnexpectedEnd
            ))
        ));
        assert!(matches!(
            frame::read::<ControlToBrain>(&mut &65537_u32.to_le_bytes()[..], Pipe::Control),
            Err(FrameError::TooLarge {
                len: 65537,
                max: 65536
            })
        ));
    }

    #[test]
    fn stage_metadata_has_literal_names_bounds_and_redacted_tap() {
        use crate::constants::{CredentialLayout, generation_hex, generation_target};
        use crate::message::{ControlToBrain, ControlToShell};
        use crate::tap::Tapped as _;
        let id = KeyStageId([0xab; 16]);
        for (len, chunks, last) in [(2000, 1, 2000), (2048, 2, 48)] {
            let plan = KeyStagePlan { id, len };
            assert_eq!(format!("{plan:?}"), "KeyStagePlan(***)");
            let layout = plan.layout().expect("bounded plan");
            assert_eq!(layout.chunks(), chunks);
            assert_eq!(layout.chunk_len(chunks - 1), last);
            assert_eq!(
                generation_target("test", "anthropic", &generation_hex(&id.0), 0),
                "test/gen/anthropic/abababababababababababababababab/0"
            );
            assert_eq!(
                generation_target("test", "anthropic", &generation_hex(&id.0), 1),
                "test/gen/anthropic/abababababababababababababababab/1"
            );
            let reply = ControlToShell::AnthropicKeyStage {
                request: 7,
                result: Ok(plan),
            };
            assert_eq!(
                postcard::from_bytes::<ControlToShell>(
                    &postcard::to_stdvec(&reply).expect("encode")
                )
                .expect("decode"),
                reply
            );
            assert!(!reply.content_free());
        }
        assert_eq!(CredentialLayout::new(0).expect("zero").chunks(), 1);
        assert_eq!(CredentialLayout::new(49152).expect("limit").chunks(), 25);
        assert_eq!(CredentialLayout::new(49153), None);
        for len in [0, 15, 2049, u16::MAX] {
            assert_eq!(KeyStagePlan { id, len }.layout(), None);
        }
        assert_eq!(
            KeyStagePlan { id, len: 16 }.layout().expect("min").bytes(),
            16
        );
        for op in [
            ConnectionOp::BeginAnthropicKeyStage {
                request: 7,
                len: 2048,
            },
            ConnectionOp::AnthropicKeyStaged { plan: id },
        ] {
            let frame = ControlToBrain::Connection(op);
            assert_eq!(
                postcard::from_bytes::<ControlToBrain>(
                    &postcard::to_stdvec(&frame).expect("encode")
                )
                .expect("decode"),
                frame
            );
            assert!(!frame.content_free());
        }
        let done = ControlToShell::AnthropicKeyStageDone {
            plan: id,
            result: Err(KeyStageFailure::Changed),
        };
        assert_eq!(
            postcard::from_bytes::<ControlToShell>(&postcard::to_stdvec(&done).expect("encode"))
                .expect("decode"),
            done
        );
        assert!(!done.content_free());
        assert_eq!(format!("{id:?}"), "KeyStageId(***)");
    }

    #[test]
    fn anthropic_keys_trim_boundaries_and_accept_only_console_prefixes() {
        for key in [
            "sk-ant-api123456".to_owned(),
            "sk-ant-usr-fake-personal-key".to_owned(),
            "sk-ant-svc-fake-service-key".to_owned(),
            format!("sk-ant-api{}", "x".repeat(2038)),
        ] {
            let padded = format!("\u{a0}\r\n{key}\t\u{2003}");
            assert_eq!(validate_anthropic_key(&padded), Ok(key.as_str()));
        }
    }

    #[test]
    fn anthropic_keys_refuse_bad_sizes_bytes_and_prefixes() {
        for text in [
            String::new(),
            " \t\r\n".to_owned(),
            "sk-ant-api12345".to_owned(),
            format!("sk-ant-api{}", "x".repeat(2039)),
            "sk-test-fake-key-only".to_owned(),
            "sk-ant-usrXfake-key-only".to_owned(),
            "sk-ant-svcXfake-key-only".to_owned(),
            "sk-ant-api-fake key".to_owned(),
            "sk-ant-api-fake\tkey".to_owned(),
            "sk-ant-api-fake\nkey".to_owned(),
            "sk-ant-api-fake\u{a0}key".to_owned(),
            "sk-ant-api-fake-é".to_owned(),
            "sk-ant-api-fake\0key".to_owned(),
            "sk-ant-api-fake\u{7f}key".to_owned(),
        ] {
            assert_eq!(validate_anthropic_key(&text), Err(KeyRejection::Format));
        }
        for (text, reason) in [
            ("sk-ant-oat", KeyRejection::SubscriptionToken),
            (" sk-ant-oat01-fake-token ", KeyRejection::SubscriptionToken),
            ("sk-ant-admin", KeyRejection::AdminKey),
            ("sk-ant-admin-fake-key", KeyRejection::AdminKey),
        ] {
            assert_eq!(validate_anthropic_key(text), Err(reason));
        }
    }

    #[test]
    fn claude_codes_last_views_keep_their_wire_tags() {
        for (view, tag) in [
            (ClaudeCodeView::Unavailable, 10),
            (ClaudeCodeView::Finding, 11),
            (ClaudeCodeView::Busy, 12),
        ] {
            assert_eq!(
                postcard::to_stdvec(&view).expect("encode"),
                [tag],
                "{view:?}"
            );
        }
        assert!(postcard::from_bytes::<ClaudeCodeView>(&[13]).is_err());
    }

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
