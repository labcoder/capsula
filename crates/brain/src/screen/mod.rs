//! Killable screen work, isolated from the brain's async runtime.

mod actor;
mod capture;
mod client;
mod context;
#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "macOS adapter follows Windows")
)]
mod elements;
mod encode;
mod frame;
mod ink;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "masks wait for the study after P3 (OSC-775)")
)]
mod mask;
mod native;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "OCR waits for the study after P3 (OSC-775)")
)]
pub(crate) mod ocr;
mod reader;
mod sanitize;
mod wire;

pub use crate::supervise::{Generation, WorkerLost};
pub use actor::serve;
pub(crate) use capture::{Acquired, Capturer, Failure, Target};
pub use client::HoldClient;
pub(crate) use client::{Closed, MAX_TTL, TypedHeld};
#[cfg(test)]
pub(crate) use client::{Holds, hold_frame, release_next};
pub(crate) use context::ScreenContext;
#[cfg(all(windows, any(test, feature = "test-support")))]
pub(crate) use elements::WalkJob;
pub use elements::{Availability, Cut, Element, ElementId, ElementTarget, ElementTree};
#[cfg(windows)]
pub(crate) use elements::{
    ElementPlatform, ElementReader, ElementSource, LIMITS, NodeProps, SourceError, TopWindow, Wake,
    excluded_image,
};
pub(crate) use encode::JPEG_CAP;
pub(crate) use frame::RawFrame;
pub(crate) use ink::InkBounds;
#[cfg(all(windows, any(test, feature = "test-support")))]
pub(crate) use mask::MaskSet;
pub use mask::Unavailable;
#[cfg(windows)]
pub(crate) use ocr::PAGE_CLASS;
#[cfg(all(test, windows))]
pub(crate) use reader::Walk;
#[cfg(windows)]
pub(crate) use reader::{CONNECTION_TIMEOUT, TRANSACTION_TIMEOUT, ThreadedReader};
#[cfg(test)]
pub(crate) use wire::FrameMeta;
#[cfg(debug_assertions)]
pub use wire::WorkerFault;
pub(crate) use wire::{CaptureId, Refusal};
