//! Optional local voice: models installed only on an explicit request, speech input and spoken
//! replies.

pub(crate) mod download;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "slice 7's 7b feeds runs' text through it")
)]
pub(crate) mod segment;
pub(crate) mod stt;
pub(crate) mod tts;
