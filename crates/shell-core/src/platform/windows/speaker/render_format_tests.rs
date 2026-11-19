use std::mem::ManuallyDrop;

use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::core::GUID;

use super::{Mix, Render, read_mix};
use crate::platform::windows::microphone::read_format;

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

fn writer(mix: &Mix) -> Option<Render> {
    let (format, mask) = read_mix(mix).ok()?;
    format.render(mask)
}

#[test]
fn rez_surround_mix_renders_only_the_front_pair_and_is_not_a_microphone_format() {
    let mut wave = extensible(8, 32, 0x063f, 3);
    let stride = usize::from(wave.Format.nBlockAlign);
    let mix = ManuallyDrop::new(Mix(&raw mut wave.Format));
    assert!(matches!(
        read_format(&mix),
        Err(crate::platform::microphone::MicError::Format)
    ));
    let render = writer(&mix).expect("the Rez speaker mix is supported");
    assert_eq!(render.frame(), stride);
    let mut out = vec![0xaa; 2 * stride];
    render.write(&[16_384, i16::MIN], &mut out);
    assert_eq!(
        out,
        [
            0, 0, 0, 0x3f, 0, 0, 0, 0x3f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0x80, 0xbf, 0, 0, 0x80, 0xbf, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    );
}

#[test]
fn five_point_one_pcm16_routes_the_same_way_for_side_and_back_masks() {
    for mask in [0x060f, 0x003f] {
        let mut wave = extensible(6, 16, mask, 1);
        let stride = usize::from(wave.Format.nBlockAlign);
        let mix = ManuallyDrop::new(Mix(&raw mut wave.Format));
        let render = writer(&mix).expect("5.1 speaker mix");
        assert_eq!(render.frame(), stride);
        let mut out = vec![0xaa; 2 * stride];
        render.write(&[0x1234, -256], &mut out);
        assert_eq!(
            out,
            [
                0x34, 0x12, 0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0, 0xff, 0, 0, 0, 0, 0, 0,
                0, 0
            ]
        );
    }
}

#[test]
fn render_refuses_unmapped_excess_or_inconsistent_layouts() {
    for (channels, mask) in [
        (6, 0),
        (6, 0x003b),
        (6, 0x007e),
        (6, 0x8000_001f),
        (9, 0x01ff),
    ] {
        let mut wave = extensible(channels, 32, mask, 3);
        let mix = ManuallyDrop::new(Mix(&raw mut wave.Format));
        assert_eq!(writer(&mix), None, "channels={channels} mask={mask:#x}");
    }
    let mut wave = extensible(6, 32, 0, 3).Format;
    wave.wFormatTag = 3;
    wave.cbSize = 0;
    let mix = ManuallyDrop::new(Mix(&raw mut wave));
    assert_eq!(writer(&mix), None, "plain multichannel WAVEFORMATEX");

    let mut wave = extensible(6, 32, 0x003f, 3);
    wave.Samples = WAVEFORMATEXTENSIBLE_0 {
        wValidBitsPerSample: 24,
    };
    let mix = ManuallyDrop::new(Mix(&raw mut wave.Format));
    assert_eq!(writer(&mix), None, "valid bits below container bits");
}

#[test]
fn mono_and_stereo_use_every_lane_without_putting_the_mask_in_capture_equality() {
    let mut mono = extensible(1, 16, 0x0004, 1);
    let mono = ManuallyDrop::new(Mix(&raw mut mono.Format));
    let mut out = [0xaa; 2];
    writer(&mono).expect("mono").write(&[0x1234], &mut out);
    assert_eq!(out, [0x34, 0x12]);

    let mut back = extensible(2, 16, 0x0030, 1);
    let back = ManuallyDrop::new(Mix(&raw mut back.Format));
    let mut front = extensible(2, 16, 0x0003, 1);
    let front = ManuallyDrop::new(Mix(&raw mut front.Format));
    assert_eq!(
        read_format(&back).expect("back stereo capture"),
        read_format(&front).expect("front stereo capture")
    );
    let mut out = [0xaa; 4];
    writer(&back).expect("back stereo").write(&[-256], &mut out);
    assert_eq!(out, [0, 0xff, 0, 0xff]);
}
