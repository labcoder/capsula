//! Killable screen work, isolated from the brain's async runtime.

mod capture;
#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "macOS adapter follows Windows")
)]
mod elements;
mod encode;
mod frame;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "A3 and A6 populate shared capture masks")
)]
mod mask;
#[cfg_attr(not(test), expect(dead_code, reason = "A6 connects regional OCR"))]
pub(crate) mod ocr;
mod reader;
mod sanitize;
mod supervisor;
mod wire;
mod worker;

pub use crate::supervise::{Generation, WorkerLost};
pub(crate) use capture::{Acquired, Capturer, Failure};
#[cfg(all(windows, any(test, feature = "test-support")))]
pub(crate) use elements::WalkJob;
pub use elements::{Availability, Cut, Element, ElementId, ElementTarget, ElementTree};
#[cfg(windows)]
pub(crate) use elements::{
    ElementPlatform, ElementReader, ElementSource, LIMITS, NodeProps, SourceError, TopWindow, Wake,
    excluded_image,
};
pub(crate) use frame::{RawFrame, display_edges};
#[cfg(all(windows, any(test, feature = "test-support")))]
pub(crate) use mask::MaskSet;
pub use mask::Unavailable;
#[cfg(windows)]
pub(crate) use reader::{CONNECTION_TIMEOUT, TRANSACTION_TIMEOUT, ThreadedReader};
pub use supervisor::{CaptureError, REQUEST_DEADLINE, ScreenHandle, ScreenSupervisor};
#[cfg(debug_assertions)]
pub use wire::WorkerFault;
pub use wire::{
    CaptureId, Exclusion, FrameMeta, Refusal, ReplyBody, RequestBody, ScreenError, TicketWire,
};
pub use worker::serve;
