//! The pipe tap: a dev-only recording of the brain's control frames, which `cargo xtask
//! pipe-dump` prints. It fails closed: a record holds the frame's op name and length, and its
//! fields only when the op is on an explicit content-free list, so a new op stays redacted until
//! someone lists it. An unlisted message is never serialized for the tap, and staging records
//! omit the wire length by storing zero.
//!
//! A recording starts with [`MAGIC`]. Each record is the pipe, the direction, the op name (a
//! length byte, then ASCII), the payload's `u32` little-endian length, a flag byte, and when the
//! flag is 1, the payload.

use std::io::{self, Read, Write};

use crate::frame::{FrameError, HEADER_LEN, Pipe, decode, encode};
use crate::message::{
    AccountOp, ConnectionOp, ConnectionsView, ControlToBrain, ControlToShell, DataToBrain,
    DataToShell, VoiceView,
};

/// The start of every recording, with the format's version.
pub const MAGIC: [u8; 5] = *b"KTAP\x02";

/// The longest op name.
const MAX_OP: usize = 32;

/// Which way a frame travelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Shell to brain.
    ToBrain,
    /// Brain to shell.
    ToShell,
}

/// A message the tap can classify: its op's name, and whether its fields are content-free.
pub trait Tapped: serde::Serialize {
    /// The variant's name.
    fn op(&self) -> &'static str;
    /// Whether the fields carry no user content, secret or identity. False for anything not
    /// listed, so the default is to redact.
    fn content_free(&self) -> bool;
}

impl Tapped for ControlToBrain {
    fn op(&self) -> &'static str {
        match self {
            Self::Hello(_) => "Hello",
            Self::QueuedHello(_) => "QueuedHello",
            Self::Quitting => "Quitting",
            Self::Stop { .. } => "Stop",
            Self::Pong => "Pong",
            Self::ClearDrawing { .. } => "ClearDrawing",
            Self::SceneFailed { .. } => "SceneFailed",
            Self::DeleteThread { .. } => "DeleteThread",
            Self::Settings(_) => "Settings",
            Self::ThreadOp(_) => "ThreadOp",
            Self::StopScenes { .. } => "StopScenes",
            Self::StepShown { .. } => "StepShown",
            Self::StepFailed { .. } => "StepFailed",
            Self::AccountOp(_) => "AccountOp",
            Self::HoldRecognized { .. } => "HoldRecognized",
            Self::KeyUp { .. } => "KeyUp",
            Self::GestureCancel { .. } => "GestureCancel",
            Self::FocusHint { .. } => "FocusHint",
            Self::ExclusionLapsed { .. } => "ExclusionLapsed",
            Self::Connection(ConnectionOp::BeginAnthropicKeyStage { .. }) => {
                "BeginAnthropicKeyStage"
            }
            Self::Connection(ConnectionOp::AnthropicKeyStaged { .. }) => "AnthropicKeyStaged",
            Self::Connection(_) => "Connection",
            Self::Warm { .. } => "Warm",
            Self::KeyDown { .. } => "KeyDown",
            Self::SpeechProgress { .. } => "SpeechProgress",
            Self::SpeechStopped { .. } => "SpeechStopped",
            Self::SegmentPlayed { .. } => "SegmentPlayed",
            Self::SpeechRetired(_) => "SpeechRetired",
            Self::AudioOutput { .. } => "AudioOutput",
            Self::QueuedAudioOutput(..) => "QueuedAudioOutput",
            Self::SpeechDecision(..) => "SpeechDecision",
            Self::AudioOutputUnavailable { .. } => "AudioOutputUnavailable",
            Self::GitHub(_) => "GitHub",
            Self::Issue { .. } => "Issue",
            Self::VoiceHoldRecognized { .. } => "VoiceHoldRecognized",
            Self::VoiceHoldStopped { .. } => "VoiceHoldStopped",
            Self::ScreenHoldRecognized { .. } => "ScreenHoldRecognized",
            Self::ScreenRelease { .. } => "ScreenRelease",
            Self::SceneSkipped { .. } => "SceneSkipped",
            Self::StepReady { .. } => "StepReady",
        }
    }

    /// `ThreadOp` isn't listed: a rename carries the user's title.
    fn content_free(&self) -> bool {
        // Every field is named, so a field added to a listed op fails to build until reviewed.
        matches!(
            self,
            Self::Quitting
                | Self::Stop {
                    run: _,
                    revealed: _
                }
                | Self::Pong
                | Self::ClearDrawing { scene_ids: _ }
                | Self::DeleteThread {
                    thread: _,
                    after_turns: _,
                    resend: _
                }
                | Self::AccountOp(
                    AccountOp::SignIn
                        | AccountOp::CancelSignIn
                        | AccountOp::SignOut
                        | AccountOp::EnablePlan
                        | AccountOp::Retry
                )
                | Self::Connection(
                    ConnectionOp::CheckAnthropicKey
                        | ConnectionOp::ConnectClaudeCode
                        | ConnectionOp::CancelClaudeSignIn
                        | ConnectionOp::DisconnectClaudeCode
                        | ConnectionOp::FindClaudeCode
                )
                | Self::Warm { thread: _ }
        )
    }
}

impl Tapped for ControlToShell {
    fn op(&self) -> &'static str {
        match self {
            Self::Welcome { .. } => "Welcome",
            Self::BuildMismatch { .. } => "BuildMismatch",
            Self::RunState { .. } => "RunState",
            Self::Ping => "Ping",
            Self::Scenes { .. } => "Scenes",
            Self::ThreadDeleted { .. } => "ThreadDeleted",
            Self::TurnRefused { .. } => "TurnRefused",
            Self::Settings { .. } => "Settings",
            Self::Error(_) => "Error",
            Self::ScenesV1 { .. } => "ScenesV1",
            Self::NoVisual { .. } => "NoVisual",
            Self::Account(_) => "Account",
            Self::ThreadsChanged => "ThreadsChanged",
            Self::Focus { .. } => "Focus",
            Self::CaptureDone { .. } => "CaptureDone",
            Self::CaptureFailed { .. } => "CaptureFailed",
            Self::Connections(_) => "Connections",
            Self::AnthropicKeyStage { .. } => "AnthropicKeyStage",
            Self::AnthropicKeyStageDone { .. } => "AnthropicKeyStageDone",
            Self::AnthropicKeyRemoved { .. } => "AnthropicKeyRemoved",
            Self::AnthropicKeyReconciled { .. } => "AnthropicKeyReconciled",
            Self::SpeechDecisionTaken(..) => "SpeechDecisionTaken",
            Self::QueuedAudioRegistered(..) => "QueuedAudioRegistered",
            Self::AccountUnsupported(..) => "AccountUnsupported",
            Self::SpeechRevoked { .. } => "SpeechRevoked",
            Self::GitHub(_) => "GitHub",
            Self::ActionState { .. } => "ActionState",
            Self::IssueRefused { .. } => "IssueRefused",
            Self::IssueUnavailable { .. } => "IssueUnavailable",
            Self::ThreadModelSet { .. } => "ThreadModelSet",
            Self::Voice(_) => "Voice",
            Self::GitHubError(_) => "GitHubError",
            Self::DrawingReplacementFailed { .. } => "DrawingReplacementFailed",
            Self::DrawingNotReturned { .. } => "DrawingNotReturned",
            Self::VoiceHoldStatus { .. } => "VoiceHoldStatus",
            Self::VoiceHoldStopAccepted { .. } => "VoiceHoldStopAccepted",
            Self::ScreenHolds(_) => "ScreenHolds",
            Self::HoldScreen { .. } => "HoldScreen",
        }
    }

    /// `Account` isn't listed: its view carries the display email.
    fn content_free(&self) -> bool {
        // Every field is named, so a field added to a listed op fails to build until reviewed.
        matches!(
            self,
            Self::Welcome { incarnation: _, issue_epoch: _ }
                | Self::RunState {
                    run: _,
                    thread: _,
                    phase: _,
                    error: _,
                    origin: _
                }
                | Self::Ping
                | Self::Scenes { scenes: _ }
                | Self::ThreadDeleted {
                    thread: _,
                    outcome: _,
                    last_run: _
                }
                | Self::TurnRefused { thread: _, turn: _ }
                | Self::Error(_)
                | Self::GitHubError(_)
                | Self::ThreadsChanged
                | Self::Focus { run: _, thread: _ }
                // Versions, states and content-free reasons only.
                | Self::Connections(ConnectionsView {
                    anthropic: _,
                    anthropic_revision: _,
                    claude_code: _
                })
                // Part states, error keys and byte counts only.
                | Self::Voice(VoiceView {
                    speech_input: _,
                    spoken_replies: _
                })
        )
    }
}

impl Tapped for DataToBrain {
    fn op(&self) -> &'static str {
        match self {
            Self::Hello(_) => "Hello",
            Self::QueuedHello(..) => "QueuedHello",
            Self::TextSubmit { .. } => "TextSubmit",
            Self::Credit { .. } => "Credit",
            Self::PageRequest { .. } => "PageRequest",
            Self::MicPcm(_) => "MicPcm",
            Self::IssueEdit { .. } => "IssueEdit",
            Self::IssueCards { .. } => "IssueCards",
            Self::IssueCardsRequest { .. } => "IssueCardsRequest",
            Self::AskAbout { .. } => "AskAbout",
        }
    }

    /// The tap records only the data pipe's handshake, so nothing here is listed.
    fn content_free(&self) -> bool {
        false
    }
}

impl Tapped for DataToShell {
    fn op(&self) -> &'static str {
        match self {
            Self::AssistantText { .. } => "AssistantText",
            Self::StepCommit { .. } => "StepCommit",
            Self::RunEnd { .. } => "RunEnd",
            Self::ThreadList { .. } => "ThreadList",
            Self::Page { .. } => "Page",
            Self::PageFailed { .. } => "PageFailed",
            Self::SceneSnapshot { .. } => "SceneSnapshot",
            Self::ScenesRemoved { .. } => "ScenesRemoved",
            Self::ScenesReplaced { .. } => "ScenesReplaced",
            Self::SceneBegin { .. } => "SceneBegin",
            Self::SceneStepCommit { .. } => "SceneStepCommit",
            Self::SceneSnapshotV1 { .. } => "SceneSnapshotV1",
            Self::SceneCutApplied { .. } => "SceneCutApplied",
            Self::SceneEnd { .. } => "SceneEnd",
            Self::Models { .. } => "Models",
            Self::Transcript(_) => "Transcript",
            Self::SegmentQueued { .. } => "SegmentQueued",
            Self::SpeechPcm(_) => "SpeechPcm",
            Self::IssueCard(_) => "IssueCard",
            Self::IssueCardRead { .. } => "IssueCardRead",
            Self::IssueCardsDone { .. } => "IssueCardsDone",
            Self::IssueReview { .. } => "IssueReview",
            Self::HoldWords { .. } => "HoldWords",
            Self::HoldStored { .. } => "HoldStored",
            Self::EventsChanged { .. } => "EventsChanged",
        }
    }

    fn content_free(&self) -> bool {
        false
    }
}

/// One recorded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The pipe it travelled on.
    pub pipe: Pipe,
    /// Which way.
    pub direction: Direction,
    /// The op's name.
    pub op: String,
    /// The frame's payload length on the wire, or zero when only the op is recorded.
    pub len: u32,
    /// The payload, only for a content-free op.
    pub payload: Option<Vec<u8>>,
}

impl Record {
    /// Records `message`, which travelled as a `wire_len`-byte payload. Its fields are kept
    /// only when [`Tapped::content_free`] says so.
    ///
    /// # Errors
    /// Fails when the length is over the pipe's cap or a listed message doesn't encode.
    pub fn of(
        pipe: Pipe,
        direction: Direction,
        message: &impl Tapped,
        wire_len: usize,
    ) -> Result<Self, FrameError> {
        let len = capped(wire_len, pipe)?;
        let op = message.op();
        let payload = if message.content_free() {
            Some(encode(message, pipe)?[HEADER_LEN..].to_vec())
        } else {
            None
        };
        Ok(Self {
            pipe,
            direction,
            op: op.to_owned(),
            len: if op_only(op) { 0 } else { len },
            payload,
        })
    }

    /// The op and length, plus the decoded fields when the payload decodes to a listed op.
    pub fn describe(&self) -> String {
        if op_only(&self.op) {
            return self.op.clone();
        }
        let redacted = format!("{} ({} bytes)", self.op, self.len);
        let Some(payload) = &self.payload else {
            return redacted;
        };
        let decoded = match (self.pipe, self.direction) {
            (Pipe::Control, Direction::ToBrain) => {
                decode::<ControlToBrain>(payload).map(|m| listed(&m))
            }
            (Pipe::Control, Direction::ToShell) => {
                decode::<ControlToShell>(payload).map(|m| listed(&m))
            }
            (Pipe::Data, Direction::ToBrain) => decode::<DataToBrain>(payload).map(|m| listed(&m)),
            (Pipe::Data, Direction::ToShell) => decode::<DataToShell>(payload).map(|m| listed(&m)),
        };
        match decoded {
            Ok(Some(fields)) => fields,
            Ok(None) => redacted,
            Err(error) => format!("undecodable {} bytes: {error}", payload.len()),
        }
    }
}

fn op_only(op: &str) -> bool {
    matches!(
        op,
        "BeginAnthropicKeyStage"
            | "AnthropicKeyStaged"
            | "AnthropicKeyStage"
            | "AnthropicKeyStageDone"
    )
}

/// A message's `Debug`, only when it's listed: a stored flag alone never authorizes it.
fn listed<M: Tapped + std::fmt::Debug>(message: &M) -> Option<String> {
    message.content_free().then(|| format!("{message:?}"))
}

fn capped(len: usize, pipe: Pipe) -> Result<u32, FrameError> {
    let max = pipe.max_payload();
    u32::try_from(len)
        .ok()
        .filter(|_| len <= max)
        .ok_or(FrameError::TooLarge { len, max })
}

/// Starts a recording.
///
/// # Errors
/// Fails when the write fails.
pub fn write_header(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(&MAGIC)
}

/// Checks a recording's start.
///
/// # Errors
/// Fails on a cut, older or unknown header.
pub fn read_header(reader: &mut impl Read) -> Result<(), FrameError> {
    let mut magic = [0; MAGIC.len()];
    reader.read_exact(&mut magic).map_err(truncated)?;
    if magic == MAGIC {
        Ok(())
    } else {
        Err(malformed())
    }
}

/// Appends one record.
///
/// # Errors
/// Fails when a length is over its cap or the write fails.
pub fn write(writer: &mut impl Write, record: &Record) -> Result<(), FrameError> {
    let op = record.op.as_bytes();
    if op.is_empty() || op.len() > MAX_OP {
        return Err(malformed());
    }
    if let Some(payload) = &record.payload
        && payload.len() != usize::try_from(record.len).map_err(|_| malformed())?
    {
        return Err(malformed());
    }
    let pipe = match record.pipe {
        Pipe::Control => 0,
        Pipe::Data => 1,
    };
    let direction = match record.direction {
        Direction::ToBrain => 0,
        Direction::ToShell => 1,
    };
    let op_len = u8::try_from(op.len()).map_err(|_| malformed())?;
    writer.write_all(&[pipe, direction, op_len])?;
    writer.write_all(op)?;
    writer.write_all(&record.len.to_le_bytes())?;
    match &record.payload {
        Some(payload) => {
            writer.write_all(&[1])?;
            writer.write_all(payload)?;
        }
        None => writer.write_all(&[0])?,
    }
    Ok(())
}

/// Reads the next record; `None` at a clean end of the recording.
///
/// # Errors
/// Fails on a read error, an unknown pipe, direction or flag byte, a bad op name, an over-cap
/// length, or a record cut short.
pub fn read(reader: &mut impl Read) -> Result<Option<Record>, FrameError> {
    let mut kind = [0; 3];
    // Only an end before the first byte is clean; anything later is a cut record.
    match reader.read(&mut kind[..1]) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut kind[1..]).map_err(truncated)?;
    let pipe = match kind[0] {
        0 => Pipe::Control,
        1 => Pipe::Data,
        _ => return Err(malformed()),
    };
    let direction = match kind[1] {
        0 => Direction::ToBrain,
        1 => Direction::ToShell,
        _ => return Err(malformed()),
    };
    let op_len = usize::from(kind[2]);
    if op_len == 0 || op_len > MAX_OP {
        return Err(malformed());
    }
    let mut op = vec![0; op_len];
    reader.read_exact(&mut op).map_err(truncated)?;
    if !op.iter().all(u8::is_ascii_alphanumeric) {
        return Err(malformed());
    }
    let op = String::from_utf8(op).map_err(|_| malformed())?;
    let mut len = [0; 4];
    reader.read_exact(&mut len).map_err(truncated)?;
    let len = u32::from_le_bytes(len);
    let size = usize::try_from(len).map_err(|_| malformed())?;
    capped(size, pipe)?;
    let mut flag = [0; 1];
    reader.read_exact(&mut flag).map_err(truncated)?;
    let payload = match flag[0] {
        0 => None,
        1 => {
            let mut payload = vec![0; size];
            reader.read_exact(&mut payload).map_err(truncated)?;
            Some(payload)
        }
        _ => return Err(malformed()),
    };
    Ok(Some(Record {
        pipe,
        direction,
        op,
        len,
        payload,
    }))
}

/// Prints a whole recording, one numbered line per record, then the count.
///
/// # Errors
/// Fails on a bad header or record, naming the record, or when the output fails.
pub fn dump(reader: &mut impl Read, out: &mut impl Write) -> Result<u64, DumpError> {
    read_header(reader).map_err(|source| DumpError::Record { seq: 0, source })?;
    let mut seq = 0u64;
    while let Some(record) = read(reader).map_err(|source| DumpError::Record { seq, source })? {
        let arrow = match record.direction {
            Direction::ToBrain => "shell -> brain",
            Direction::ToShell => "brain -> shell",
        };
        writeln!(
            out,
            "{seq:>5} {:<7} {arrow}  {}",
            format!("{:?}", record.pipe),
            record.describe()
        )?;
        seq += 1;
    }
    writeln!(out, "{seq} frames")?;
    Ok(seq)
}

/// Why a recording couldn't be printed.
#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    /// The header or a record is bad.
    #[error("record {seq}")]
    Record {
        /// The record's number; 0 for the header.
        seq: u64,
        /// What's wrong with it.
        #[source]
        source: FrameError,
    },
    /// Writing the output failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn malformed() -> FrameError {
    FrameError::Malformed(postcard::Error::DeserializeBadEncoding)
}

fn truncated(error: io::Error) -> FrameError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(error)
    }
}

#[cfg(test)]
mod tests;
