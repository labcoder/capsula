//! The queued speaker and capture-inhibit fixture types (docs/development/speaker.md).

#[expect(
    dead_code,
    reason = "the drawing conductor and its ports have no live callers yet"
)]
pub(crate) mod conductor;
#[expect(
    dead_code,
    reason = "stock startup still selects the silent Legacy session"
)]
pub(crate) mod queued;
mod tail;

use std::time::Duration;

use kumochi_protocol::message::RunId;

pub use tail::{FAULT, RELEASED, Snapshot, Tail, UNKNOWN};

pub(crate) const DRAIN: Duration = Duration::from_millis(500);
pub(crate) const STALL: Duration = Duration::from_millis(200);

/// Commands observed by the capture-inhibit fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Stop the named run.
    Stop(RunId),
    /// Pause under the inhibit's serial.
    Pause(u32),
    /// Resume under the inhibit's serial.
    Resume(u32),
}
