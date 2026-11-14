//! What the platform code reads and the shell decides on, as plain data: no operating-system
//! calls here. The calls stay in the shell's `platform` module.

pub mod host;
pub mod input;
