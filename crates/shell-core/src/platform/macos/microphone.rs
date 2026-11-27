//! Mac input through the Core Audio HAL: the pinned stream format, the IO cycle bound, the
//! acceptance edge and cycle positions. The IO block and device ownership build on these.

use std::time::Duration;

use objc2_core_audio::AudioGetCurrentHostTime;
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonMixable,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};

use crate::platform::microphone::{Format, MicError, Sample};

mod block;

/// The longest IO cycle admitted, so a stalled device is noticed within a few drain polls.
const MAX_CYCLE: Duration = Duration::from_millis(100);
/// Scheduling slack the stall check allows on top of two cycles.
const STALL_SLACK: Duration = Duration::from_millis(100);
/// Sample times convert to whole frames exactly only up to 2^53.
const EXACT_FRAMES: f64 = 9_007_199_254_740_992.0;

/// The acceptance edge on the clock `AudioTimeStamp.mHostTime` uses.
pub(in crate::platform) fn collection_start() -> Result<u64, MicError> {
    // SAFETY: takes nothing and only reads the host clock.
    edge(unsafe { AudioGetCurrentHostTime() })
}

/// A cycle stamped one tick after the reading is ambiguous across threads, so the edge is two on.
fn edge(now: u64) -> Result<u64, MicError> {
    if now == 0 {
        return Err(MicError::Timestamp);
    }
    now.checked_add(2).ok_or(MicError::Timestamp)
}

/// Accepts the stream's virtual format only as packed, interleaved, native-endian float32 at the
/// device's nominal rate, so the IO block receives exactly what the packetizer reads. The float
/// flag is required, the packed and non-mixable flags are allowed (packing is implied when the
/// samples fill the frame, as required below), and any other flag refuses.
pub(super) fn read_format(
    asbd: &AudioStreamBasicDescription,
    nominal_rate: f64,
) -> Result<Format, MicError> {
    let flags = asbd.mFormatFlags;
    let packed_float = flags & kAudioFormatFlagIsFloat != 0
        && flags
            & !(kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonMixable)
            == 0;
    let rate = asbd.mSampleRate;
    if asbd.mFormatID != kAudioFormatLinearPCM
        || !packed_float
        || asbd.mBitsPerChannel != 32
        || asbd.mFramesPerPacket != 1
        || Some(asbd.mBytesPerFrame) != asbd.mChannelsPerFrame.checked_mul(4)
        || asbd.mBytesPerPacket != asbd.mBytesPerFrame
        || rate.to_bits() != nominal_rate.to_bits()
        || rate.fract() != 0.0
        || !(1.0..=f64::from(u32::MAX)).contains(&rate)
    {
        return Err(MicError::Format);
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::as_conversions,
        reason = "an integral rate within u32's range converts exactly"
    )]
    let rate = rate as u32;
    Format {
        rate,
        channels: u16::try_from(asbd.mChannelsPerFrame).map_err(|_| MicError::Format)?,
        sample: Sample::F32,
    }
    .check()
}

/// How long the worker waits for the next valid cycle before calling the input stalled: two
/// pinned cycles plus slack. A cycle over 100 ms refuses, so no admitted pin reads as a stall.
pub(super) fn stall_after(cycle_frames: u32, rate: u32) -> Result<Duration, MicError> {
    if cycle_frames == 0 || rate == 0 {
        return Err(MicError::Format);
    }
    let cycle =
        Duration::from_nanos((u64::from(cycle_frames) * 1_000_000_000).div_ceil(u64::from(rate)));
    if cycle > MAX_CYCLE {
        return Err(MicError::Format);
    }
    Ok(cycle * 2 + STALL_SLACK)
}

/// Stream positions of kept cycles: whole, finite and moving forward. A forward gap is left to
/// the shared missing-frame rule; anything else fails the hold.
#[derive(Clone, Debug, Default)]
pub(super) struct Positions {
    next: Option<u64>,
}

impl Positions {
    /// The cycle's first frame. A missing, infinite, negative or inexact sample time is
    /// `Timestamp`; a fractional, repeated or backward one is `Discontinuity`.
    pub(super) fn admit(&mut self, sample_time: Option<f64>, frames: u32) -> Result<u64, MicError> {
        let time = sample_time.ok_or(MicError::Timestamp)?;
        if !time.is_finite() || time < 0.0 || time > EXACT_FRAMES - f64::from(frames) {
            return Err(MicError::Timestamp);
        }
        if time.fract() != 0.0 {
            return Err(MicError::Discontinuity);
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::as_conversions,
            reason = "a whole, non-negative time below 2^53 converts exactly"
        )]
        let position = time as u64;
        if self.next.is_some_and(|next| position < next) {
            return Err(MicError::Discontinuity);
        }
        self.next = Some(position + u64::from(frames));
        Ok(position)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use objc2_core_audio_types::{
        AudioStreamBasicDescription, kAudioFormatFlagIsAlignedHigh, kAudioFormatFlagIsBigEndian,
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsNonMixable,
        kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
    };

    use super::{Positions, edge, read_format, stall_after};
    use crate::platform::microphone::{Format, MicError, PacketBoundary, PacketStatus, Sample};

    fn float(rate: f64, channels: u32) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 4 * channels,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * channels,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }

    fn flagged(flags: u32) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mFormatFlags: flags,
            ..float(48_000.0, 2)
        }
    }

    #[test]
    fn only_interleaved_native_float32_at_the_nominal_rate_is_accepted() {
        let format = |rate, channels| Format {
            rate,
            channels,
            sample: Sample::F32,
        };
        assert_eq!(
            read_format(&float(48_000.0, 1), 48_000.0),
            Ok(format(48_000, 1))
        );
        assert_eq!(
            read_format(&float(8_000.0, 2), 8_000.0),
            Ok(format(8_000, 2))
        );
        assert_eq!(
            read_format(&float(192_000.0, 2), 192_000.0),
            Ok(format(192_000, 2))
        );
        let refused = [
            ("rate differs from nominal", float(44_100.0, 1), 48_000.0),
            ("fractional rate", float(44_100.5, 1), 44_100.5),
            ("rate not a multiple of 50", float(44_125.0, 1), 44_125.0),
            ("rate below the wire bound", float(7_950.0, 1), 7_950.0),
            ("rate above the wire bound", float(192_050.0, 1), 192_050.0),
            ("not a number", float(f64::NAN, 1), f64::NAN),
            ("no channels", float(48_000.0, 0), 48_000.0),
            ("three channels", float(48_000.0, 3), 48_000.0),
        ];
        for (why, asbd, nominal) in refused {
            assert_eq!(read_format(&asbd, nominal), Err(MicError::Format), "{why}");
        }
        let layout = [
            (
                "not linear PCM",
                AudioStreamBasicDescription {
                    mFormatID: 0x6161_6320,
                    ..float(48_000.0, 1)
                },
            ),
            (
                "two frames per packet",
                AudioStreamBasicDescription {
                    mFramesPerPacket: 2,
                    ..float(48_000.0, 1)
                },
            ),
            (
                "bytes per packet mismatched",
                AudioStreamBasicDescription {
                    mBytesPerPacket: 8,
                    ..float(48_000.0, 1)
                },
            ),
            (
                "bytes per frame mismatched",
                AudioStreamBasicDescription {
                    mBytesPerFrame: 8,
                    mBytesPerPacket: 8,
                    ..float(48_000.0, 1)
                },
            ),
            (
                "64-bit samples",
                AudioStreamBasicDescription {
                    mBitsPerChannel: 64,
                    ..float(48_000.0, 1)
                },
            ),
        ];
        for (why, asbd) in layout {
            assert_eq!(read_format(&asbd, 48_000.0), Err(MicError::Format), "{why}");
        }
    }

    #[test]
    fn float_is_required_and_only_the_packed_and_non_mixable_flags_may_join_it() {
        let stereo = Ok(Format {
            rate: 48_000,
            channels: 2,
            sample: Sample::F32,
        });
        for flags in [
            kAudioFormatFlagIsFloat,
            kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonMixable,
        ] {
            assert_eq!(read_format(&flagged(flags), 48_000.0), stereo);
        }
        let float = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        for flags in [
            kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
            kAudioFormatFlagIsPacked,
            float | kAudioFormatFlagIsSignedInteger,
            float | kAudioFormatFlagIsBigEndian,
            float | kAudioFormatFlagIsNonInterleaved,
            float | kAudioFormatFlagIsAlignedHigh,
            float | 1 << 31,
        ] {
            assert_eq!(
                read_format(&flagged(flags), 48_000.0),
                Err(MicError::Format),
                "{flags:#x}"
            );
        }
    }

    #[test]
    fn a_cycle_over_100_ms_refuses_and_the_stall_bound_follows_the_cycle() {
        assert_eq!(stall_after(4_800, 48_000), Ok(Duration::from_millis(300)));
        assert_eq!(stall_after(4_801, 48_000), Err(MicError::Format));
        assert_eq!(stall_after(2_048, 8_000), Err(MicError::Format));
        assert_eq!(
            stall_after(512, 48_000),
            Ok(Duration::from_nanos(121_333_334))
        );
        assert_eq!(
            stall_after(1, 192_000),
            Ok(Duration::from_nanos(100_010_418))
        );
        assert_eq!(stall_after(0, 48_000), Err(MicError::Format));
        assert_eq!(stall_after(512, 0), Err(MicError::Format));
    }

    #[test]
    fn the_edge_skips_the_adjacent_tick_and_keeps_the_next() {
        assert_eq!(edge(1_000), Ok(1_002));
        assert_eq!(edge(0), Err(MicError::Timestamp));
        assert_eq!(edge(u64::MAX - 1), Err(MicError::Timestamp));
        assert_eq!(edge(u64::MAX - 2), Ok(u64::MAX));
        let cycle = |captured_at| PacketStatus {
            frames: 512,
            position: 0,
            captured_at,
            discontinuity: false,
            timestamp_error: false,
        };
        let mut boundary = PacketBoundary::default();
        assert_eq!(boundary.packet(Some(1_002), cycle(1_001)), Ok(false));
        assert_eq!(boundary.packet(Some(1_002), cycle(1_002)), Ok(true));
        assert_eq!(
            boundary.packet(Some(1_002), cycle(0)),
            Err(MicError::Timestamp)
        );
    }

    #[test]
    fn positions_must_be_whole_and_move_forward() {
        let mut positions = Positions::default();
        assert_eq!(positions.admit(Some(1_024.0), 512), Ok(1_024));
        assert_eq!(positions.admit(Some(1_536.0), 512), Ok(1_536));
        assert_eq!(positions.admit(Some(4_096.0), 512), Ok(4_096));
        for repeated in [4_096.0, 4_607.0, 1_024.0] {
            assert_eq!(
                positions.clone().admit(Some(repeated), 512),
                Err(MicError::Discontinuity)
            );
        }
        assert_eq!(
            positions.admit(Some(4_608.5), 512),
            Err(MicError::Discontinuity)
        );
        assert_eq!(positions.admit(Some(4_608.0), 512), Ok(4_608));
        let mut fresh = Positions::default();
        assert_eq!(fresh.admit(None, 512), Err(MicError::Timestamp));
        assert_eq!(
            fresh.admit(Some(f64::INFINITY), 512),
            Err(MicError::Timestamp)
        );
        assert_eq!(fresh.admit(Some(f64::NAN), 512), Err(MicError::Timestamp));
        assert_eq!(fresh.admit(Some(-512.0), 512), Err(MicError::Timestamp));
        assert_eq!(
            fresh.admit(Some(9_007_199_254_740_480.0), 512),
            Ok(9_007_199_254_740_480)
        );
        assert_eq!(
            Positions::default().admit(Some(9_007_199_254_740_481.0), 512),
            Err(MicError::Timestamp)
        );
    }
}
