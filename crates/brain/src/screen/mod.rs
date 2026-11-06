//! Killable screen work, isolated from the brain's async runtime.

mod capture;
#[cfg_attr(not(test), expect(dead_code, reason = "A3b opens the platform reader"))]
mod elements;
mod encode;
mod frame;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "A3 and A6 populate shared capture masks")
)]
mod mask;
mod reader;
mod sanitize;
mod supervisor;
mod wire;
mod worker;

pub use crate::supervise::{Generation, WorkerLost};
pub(crate) use capture::{Acquired, Capturer, Failure};
pub use elements::{Availability, Cut, Element, ElementId, ElementTarget, ElementTree};
pub(crate) use frame::{RawFrame, display_edges};
pub use mask::Unavailable;
pub use supervisor::{CaptureError, REQUEST_DEADLINE, ScreenHandle, ScreenSupervisor};
#[cfg(debug_assertions)]
pub use wire::WorkerFault;
pub use wire::{
    CaptureId, Exclusion, FrameMeta, Refusal, ReplyBody, RequestBody, ScreenError, TicketWire,
};
pub use worker::serve;
