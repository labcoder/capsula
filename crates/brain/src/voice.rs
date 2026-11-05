//! Optional local voice: models installed only on an explicit request, speech input and spoken
//! replies.

pub(crate) mod download;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 7's 7b2 owns one in the runtime")
)]
pub(crate) mod pump;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 7's 7b drives a run's speech through it")
)]
pub(crate) mod reply;
pub(crate) mod resample;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 7's 7b feeds runs' text through it")
)]
pub(crate) mod segment;
pub(crate) mod stt;
pub(crate) mod tts;
