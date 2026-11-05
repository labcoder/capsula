//! Killable screen work, isolated from the brain's async runtime.

mod capture;
mod frame;
mod supervisor;
mod wire;
mod worker;

pub use crate::supervise::WorkerLost;
pub(crate) use capture::{Acquired, Capturer, Failure};
pub(crate) use frame::{RawFrame, display_edges};
pub use supervisor::{CaptureError, REQUEST_DEADLINE, ScreenHandle, ScreenSupervisor};
#[cfg(debug_assertions)]
pub use wire::WorkerFault;
pub use wire::{
    CaptureId, Exclusion, FrameMeta, Refusal, ReplyBody, RequestBody, ScreenError, TicketWire,
};
pub use worker::serve;
