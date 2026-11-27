//! The IO block's side of Mac capture: per call, one cycle validated against the pin and copied
//! into the ring while collecting, with no allocation, lock, wait or formatting on Core Audio's
//! real-time thread.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use block2::RcBlock;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};

use crate::platform::macos::ring::{Produced, Ring, Stamp};
use crate::platform::microphone::{Generation, MicError, copy_batch};

const NO_FAULT: u8 = 0;
const FORMAT: u8 = 1;
const OVERFLOW: u8 = 2;

/// The block Core Audio calls with `(now, input, input time, output, output time)`.
pub(super) type IoBlock = RcBlock<
    dyn Fn(
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
    ),
>;

/// Shared by the IO block and the capture worker. The block's clone lives inside Core Audio
/// until its IO proc is destroyed, so neither the ring nor the gate can be freed under a cycle.
pub(super) struct Context {
    token: Generation,
    session: Arc<AtomicBool>,
    channels: u32,
    frames: u32,
    bytes: u32,
    ring: Ring,
    /// Sticky once set: any block-side failure ends the hold.
    fault: AtomicU8,
    /// Cycles that passed validation, collecting or not: proof that input flows.
    valid_cycles: AtomicU64,
}

impl Context {
    pub(super) fn new(
        token: Generation,
        session: Arc<AtomicBool>,
        rate: u32,
        channels: u16,
        frames: u32,
    ) -> Result<Self, MicError> {
        let ring = Ring::new(rate, channels, frames)?;
        let channels = u32::from(channels);
        let bytes = frames.checked_mul(4 * channels).ok_or(MicError::Format)?;
        Ok(Self {
            token,
            session,
            channels,
            frames,
            bytes,
            ring,
            fault: AtomicU8::new(NO_FAULT),
            valid_cycles: AtomicU64::new(0),
        })
    }

    pub(super) fn ring(&self) -> &Ring {
        &self.ring
    }

    pub(super) fn fault(&self) -> Result<(), MicError> {
        match self.fault.load(Ordering::Acquire) {
            NO_FAULT => Ok(()),
            FORMAT => Err(MicError::Format),
            _ => Err(MicError::Overflow),
        }
    }

    pub(super) fn valid_cycles(&self) -> u64 {
        self.valid_cycles.load(Ordering::Acquire)
    }
}

/// The IO block to register for this context; it holds one clone of it.
pub(super) fn io_block(context: Arc<Context>) -> IoBlock {
    RcBlock::new(
        move |_: NonNull<AudioTimeStamp>,
              input: NonNull<AudioBufferList>,
              input_time: NonNull<AudioTimeStamp>,
              _: NonNull<AudioBufferList>,
              _: NonNull<AudioTimeStamp>| {
            // SAFETY: Core Audio passes this cycle's input buffer list, valid for the call.
            let input = unsafe { input.as_ref() };
            // SAFETY: Core Audio passes this cycle's input timestamp, valid for the call.
            let input_time = unsafe { input_time.as_ref() };
            on_io(&context, input, input_time);
        },
    )
}

/// One IO cycle. A buffer that doesn't match the pin sets a sticky `Format` fault and copies
/// nothing; a valid one counts as progress and is copied in place only while collecting. A full
/// ring sets a sticky `Overflow` fault. A missing host time is stamped 0 and a missing sample
/// time NaN, so the worker fails them after acceptance.
fn on_io(context: &Context, input: &AudioBufferList, time: &AudioTimeStamp) {
    let [buffer] = &input.mBuffers;
    if input.mNumberBuffers != 1
        || buffer.mNumberChannels != context.channels
        || buffer.mData.is_null()
        || buffer.mDataByteSize != context.bytes
    {
        context.fault.fetch_max(FORMAT, Ordering::AcqRel);
        return;
    }
    context.valid_cycles.fetch_add(1, Ordering::AcqRel);
    let stamp = Stamp {
        frames: context.frames,
        host_time: if time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
            time.mHostTime
        } else {
            0
        },
        sample_time: if time.mFlags.contains(AudioTimeStampFlags::SampleTimeValid) {
            time.mSampleTime
        } else {
            f64::NAN
        },
    };
    let Ok(len) = usize::try_from(buffer.mDataByteSize) else {
        return;
    };
    // SAFETY: Core Audio owns `mDataByteSize` readable bytes at the non-null `mData` for the
    // duration of this call, and nothing here outlives it.
    let data = unsafe { std::slice::from_raw_parts(buffer.mData.cast::<u8>(), len) };
    let produced = context.ring.produce(stamp, |raw| {
        if raw.len() != data.len() {
            return 0;
        }
        copy_batch(
            raw,
            Some(data),
            0,
            || context.token.collecting(&context.session),
            #[cfg(any(test, feature = "test-support"))]
            || {},
        )
    });
    if produced == Produced::Full {
        context.fault.fetch_max(OVERFLOW, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use kumochi_protocol::message::GestureId;
    use objc2_core_audio_types::{
        AudioBuffer, AudioBufferList, AudioTimeStamp, AudioTimeStampFlags, SMPTETime,
        SMPTETimeFlags, SMPTETimeType,
    };

    use super::{Context, io_block, on_io};
    use crate::platform::macos::ring::Stamp;
    use crate::platform::microphone::{CollectionGate, Generation, MicError};

    /// 50 Hz mono, four-frame cycles: two 16-byte slots.
    fn context() -> (CollectionGate, Arc<Context>) {
        let gate = CollectionGate::default();
        let token = Generation::collecting_for_tests(&gate, GestureId(1)).unwrap();
        let context = Context::new(token, Arc::new(AtomicBool::new(true)), 50, 1, 4).unwrap();
        (gate, Arc::new(context))
    }

    fn time(flags: AudioTimeStampFlags) -> AudioTimeStamp {
        AudioTimeStamp {
            mSampleTime: 512.0,
            mHostTime: 7_000,
            mRateScalar: 1.0,
            mWordClockTime: 0,
            mSMPTETime: SMPTETime {
                mSubframes: 0,
                mSubframeDivisor: 0,
                mCounter: 0,
                mType: SMPTETimeType(0),
                mFlags: SMPTETimeFlags::empty(),
                mHours: 0,
                mMinutes: 0,
                mSeconds: 0,
                mFrames: 0,
            },
            mFlags: flags,
            mReserved: 0,
        }
    }

    fn both() -> AudioTimeStamp {
        time(AudioTimeStampFlags::HostTimeValid | AudioTimeStampFlags::SampleTimeValid)
    }

    fn list(buffers: u32, channels: u32, bytes: u32, data: *mut c_void) -> AudioBufferList {
        AudioBufferList {
            mNumberBuffers: buffers,
            mBuffers: [AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: bytes,
                mData: data,
            }],
        }
    }

    fn popped(context: &Context) -> Option<(Stamp, Vec<u8>)> {
        context.ring().pop(|stamp, bytes| (stamp, bytes.to_vec()))
    }

    #[test]
    fn a_valid_cycle_is_counted_and_copied_only_while_collecting() {
        let (gate, context) = context();
        let mut audio = [0xa5_u8; 16];
        let input = list(1, 1, 16, audio.as_mut_ptr().cast());
        on_io(&context, &input, &both());
        assert_eq!(context.valid_cycles(), 1);
        let stamp = Stamp {
            frames: 4,
            host_time: 7_000,
            sample_time: 512.0,
        };
        assert_eq!(popped(&context), Some((stamp, vec![0xa5; 16])));
        gate.release(GestureId(1));
        on_io(&context, &input, &both());
        assert_eq!(context.valid_cycles(), 2);
        assert_eq!(popped(&context), None);
        assert_eq!(context.fault(), Ok(()));
    }

    #[test]
    fn a_buffer_off_the_pin_faults_format_and_copies_nothing() {
        let mut audio = [0xa5_u8; 32];
        let data: *mut c_void = audio.as_mut_ptr().cast();
        let cases = [
            ("two buffers", list(2, 1, 16, data)),
            ("no buffers", list(0, 1, 16, data)),
            ("two channels", list(1, 2, 16, data)),
            ("null data", list(1, 1, 16, std::ptr::null_mut())),
            ("a partial frame", list(1, 1, 15, data)),
            ("a short cycle", list(1, 1, 12, data)),
            ("a long cycle", list(1, 1, 20, data)),
        ];
        for (why, input) in cases {
            let (_gate, context) = context();
            on_io(&context, &input, &both());
            assert_eq!(context.fault(), Err(MicError::Format), "{why}");
            assert_eq!(context.valid_cycles(), 0, "{why}");
            assert_eq!(popped(&context), None, "{why}");
            on_io(&context, &list(1, 1, 16, data), &both());
            assert_eq!(context.fault(), Err(MicError::Format), "{why} stays");
        }
    }

    #[test]
    fn a_full_ring_faults_overflow_for_good() {
        let (_gate, context) = context();
        let mut audio = [0xa5_u8; 16];
        let input = list(1, 1, 16, audio.as_mut_ptr().cast());
        on_io(&context, &input, &both());
        on_io(&context, &input, &both());
        assert_eq!(context.fault(), Ok(()));
        on_io(&context, &input, &both());
        assert_eq!(context.fault(), Err(MicError::Overflow));
        assert_eq!(
            popped(&context).map(|(_, bytes)| bytes),
            Some(vec![0xa5; 16])
        );
        on_io(&context, &input, &both());
        assert_eq!(context.fault(), Err(MicError::Overflow));
    }

    #[test]
    fn missing_timestamp_flags_mark_the_stamp_for_the_worker_to_refuse() {
        let (_gate, context) = context();
        let mut audio = [0xa5_u8; 16];
        let input = list(1, 1, 16, audio.as_mut_ptr().cast());
        on_io(
            &context,
            &input,
            &time(AudioTimeStampFlags::SampleTimeValid),
        );
        on_io(&context, &input, &time(AudioTimeStampFlags::HostTimeValid));
        let first = popped(&context).unwrap().0;
        assert_eq!((first.host_time, first.sample_time), (0, 512.0));
        let second = popped(&context).unwrap().0;
        assert_eq!(second.host_time, 7_000);
        assert!(second.sample_time.is_nan());
    }

    #[test]
    fn the_block_owns_one_context_reference_until_it_is_dropped() {
        const fn shared<T: Send + Sync>() {}
        shared::<Context>();
        let (_gate, context) = context();
        let block = io_block(Arc::clone(&context));
        assert_eq!(Arc::strong_count(&context), 2);
        let mut audio = [0xa5_u8; 16];
        let mut input = list(1, 1, 16, audio.as_mut_ptr().cast());
        let mut stamp = both();
        let mut output = list(0, 0, 0, std::ptr::null_mut());
        let mut now = both();
        let mut later = both();
        block.call((
            NonNull::from(&mut now),
            NonNull::from(&mut input),
            NonNull::from(&mut stamp),
            NonNull::from(&mut output),
            NonNull::from(&mut later),
        ));
        assert_eq!(
            popped(&context).map(|(_, bytes)| bytes),
            Some(vec![0xa5; 16])
        );
        drop(block);
        assert_eq!(Arc::strong_count(&context), 1);
    }
}
