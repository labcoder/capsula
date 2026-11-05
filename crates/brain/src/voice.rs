//! Optional local voice: models installed only on an explicit request, speech input and spoken
//! replies.

pub(crate) mod download;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "slice 7's PR 8 starts one when spoken replies are installed"
    )
)]
pub(crate) mod pump;
pub(crate) mod reply;
pub(crate) mod resample;
pub(crate) mod segment;
pub(crate) mod stt;
pub(crate) mod tts;
