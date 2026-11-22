use std::mem::ManuallyDrop;
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::core::GUID;

use super::{Mix, read_format};
use crate::platform::microphone::MicError;

fn extensible(channels: u16, bits: u16, mask: u32, subtype: u32) -> WAVEFORMATEXTENSIBLE {
    let align = channels * (bits / 8);
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: 0xfffe,
            nChannels: channels,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * u32::from(align),
            nBlockAlign: align,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits,
        },
        dwChannelMask: mask,
        SubFormat: GUID::from_u128((u128::from(subtype) << 96) | 0x0000_0010_8000_00aa_0038_9b71),
    }
}

#[test]
fn surround_mix_is_not_a_microphone_format() {
    let mut wave = extensible(8, 32, 0x063f, 3);
    let mix = ManuallyDrop::new(Mix(&raw mut wave.Format));
    assert_eq!(read_format(&mix), Err(MicError::Format));
}

#[test]
fn stereo_channel_mask_does_not_affect_capture_format_equality() {
    let mut back = extensible(2, 16, 0x0030, 1);
    let back = ManuallyDrop::new(Mix(&raw mut back.Format));
    let mut front = extensible(2, 16, 0x0003, 1);
    let front = ManuallyDrop::new(Mix(&raw mut front.Format));
    assert_eq!(
        read_format(&back).expect("back stereo capture"),
        read_format(&front).expect("front stereo capture")
    );
}
