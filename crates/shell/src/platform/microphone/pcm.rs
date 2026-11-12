use std::fmt;

use async_channel::Sender;
use kumochi_protocol::message::{GestureId, MAX_HOLD_CHUNKS, MAX_RATE, MIN_RATE, MicPcm};
use zeroize::Zeroize;

use super::MicError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sample {
    U8,
    I16,
    I24,
    I32,
    F32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Format {
    pub(crate) rate: u32,
    pub(crate) channels: u16,
    pub(crate) sample: Sample,
}

impl Format {
    pub(crate) fn check(self) -> Result<Self, MicError> {
        if !(MIN_RATE..=MAX_RATE).contains(&self.rate)
            || !self.rate.is_multiple_of(50)
            || !(1..=2).contains(&self.channels)
        {
            return Err(MicError::Format);
        }
        Ok(self)
    }

    pub(crate) fn bytes(self) -> usize {
        match self.sample {
            Sample::U8 => 1,
            Sample::I16 => 2,
            Sample::I24 => 3,
            Sample::I32 | Sample::F32 => 4,
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::as_conversions,
        reason = "finite clamped PCM conversion fits i16"
    )]
    pub(crate) fn mono(self, frame: &[u8]) -> Result<i16, MicError> {
        if frame.len() != self.bytes() * usize::from(self.channels) {
            return Err(MicError::Format);
        }
        let mut sum = 0.0_f64;
        for channel in frame.chunks_exact(self.bytes()) {
            sum += match self.sample {
                Sample::U8 => f64::from(i16::from(channel[0]) - 128) * 256.0,
                Sample::I16 => f64::from(i16::from_le_bytes([channel[0], channel[1]])),
                Sample::I24 => {
                    f64::from(i32::from_le_bytes([0, channel[0], channel[1], channel[2]])) / 65536.0
                }
                Sample::I32 => {
                    f64::from(i32::from_le_bytes([
                        channel[0], channel[1], channel[2], channel[3],
                    ])) / 65536.0
                }
                Sample::F32 => {
                    let value =
                        f32::from_le_bytes([channel[0], channel[1], channel[2], channel[3]]);
                    if !value.is_finite() {
                        return Err(MicError::Format);
                    }
                    f64::from(value.clamp(-1.0, 1.0)) * 32768.0
                }
            };
        }
        Ok((sum / f64::from(self.channels)).clamp(-32768.0, 32767.0) as i16)
    }
}

pub(crate) struct Packetizer {
    gesture: GestureId,
    rate: u32,
    held: Vec<i16>,
    count: u32,
    high_water: usize,
    failed: bool,
}

impl fmt::Debug for Packetizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packetizer")
            .field("count", &self.count)
            .field("held_samples", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl Packetizer {
    pub(crate) fn new(gesture: GestureId, format: Format) -> Result<Self, MicError> {
        format.check()?;
        Ok(Self {
            gesture,
            rate: format.rate,
            held: Vec::with_capacity(
                usize::try_from(format.rate / 50).map_err(|_| MicError::Format)?,
            ),
            count: 0,
            high_water: 0,
            failed: false,
        })
    }

    pub(crate) fn push(&mut self, sample: i16, output: &Sender<MicPcm>) -> Result<(), MicError> {
        if self.failed {
            return Err(MicError::Overflow);
        }
        if self.held.len() == self.full() {
            self.emit(false, output)?;
        }
        if self.count == MAX_HOLD_CHUNKS {
            self.clear();
            return Err(MicError::Overflow);
        }
        self.held.push(sample);
        Ok(())
    }

    pub(crate) fn finish(&mut self, output: &Sender<MicPcm>) -> Result<u32, MicError> {
        if self.failed {
            return Err(MicError::Overflow);
        }
        if !self.held.is_empty() {
            self.emit(true, output)?;
        }
        self.failed = true;
        Ok(self.count)
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the argument supplies MicPcm's wire last bit"
    )]
    fn emit(&mut self, last: bool, output: &Sender<MicPcm>) -> Result<(), MicError> {
        if self.count >= MAX_HOLD_CHUNKS {
            self.clear();
            return Err(MicError::Overflow);
        }
        let full = self.full();
        let samples = std::mem::replace(&mut self.held, Vec::with_capacity(full));
        let packet = MicPcm {
            gesture: self.gesture,
            seq: self.count,
            rate: self.rate,
            last,
            samples,
        };
        if let Err(error) = output.try_send(packet) {
            error.into_inner().samples.zeroize();
            self.clear();
            return Err(MicError::Overflow);
        }
        self.count += 1;
        self.high_water = self.high_water.max(output.len());
        Ok(())
    }

    /// The most chunks this packetizer has seen waiting in the queue after a send.
    pub(crate) fn high_water(&self) -> usize {
        self.high_water
    }

    fn full(&self) -> usize {
        usize::try_from(self.rate / 50).expect("validated PCM packet length fits usize")
    }

    pub(crate) fn clear(&mut self) {
        self.held.zeroize();
        self.failed = true;
    }
}

impl Drop for Packetizer {
    fn drop(&mut self) {
        self.clear();
    }
}
