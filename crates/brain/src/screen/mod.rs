//! Killable screen work, isolated from the brain's async runtime.

mod supervisor;
mod wire;
mod worker;

pub use supervisor::{REQUEST_DEADLINE, ScreenHandle, ScreenSupervisor, WorkerLost};
#[cfg(debug_assertions)]
pub use wire::WorkerFault;
pub use wire::{ReplyBody, RequestBody, ScreenError};
pub use worker::serve;
