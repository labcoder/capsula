//! The speaker: one connection's output device and the played-content clock behind it
//! (docs/development/speaker.md). So far only the clock is here, and nothing opens a device.

#[expect(
    dead_code,
    reason = "the speaker's owner, added next, drives the clock"
)]
mod clock;
