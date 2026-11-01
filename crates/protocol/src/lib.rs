//! What the brain and the shell share: product constants, secret handling and, from slice 0's
//! second step, the pipe messages. Data only: this crate does no I/O.

pub mod constants;
mod secret;

pub use secret::Secret;
