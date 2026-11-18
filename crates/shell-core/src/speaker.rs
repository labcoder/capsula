//! The speaker: one connection's output device, with the played-content clock behind it
//! (docs/development/speaker.md). So far it holds the shared run owner; nothing opens one.

mod clock;
mod engine;

pub use clock::{Fault, Observation, STALL};
pub use engine::{Data, Engine, Release, Staged, Taken};
