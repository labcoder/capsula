//! Dormant output seam for the speech worker. Nothing in the normal link opens it.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{BufferSize, ErrorKind, StreamConfig};
use crossbeam_queue::ArrayQueue;
use kumochi_audio::Resampler;
use kumochi_protocol::message::{MAX_RATE, MIN_RATE};
use thiserror::Error;
use zeroize::{Zeroize as _, Zeroizing};

#[cfg(test)]
mod tests;

const SOURCE_RATE: u32 = 24_000;
const PACKET_SAMPLES: usize = 480;
const MAX_FRAMES: usize = 4_096;
const MAX_CHANNELS: u16 = 32;
const BLOCKS: usize = 8;
type Block = Zeroizing<Vec<f32>>;

/// Whether a submission ends the current source segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    /// More samples follow in this segment.
    More,
    /// Flush all remaining samples and retire this segment's converter.
    End,
}

/// An output action that could not be completed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum OutputError {
    /// The output cannot be opened or continued.
    #[error("output unavailable")]
    Unavailable,
    /// The worker must retain this packet and retry after a callback recycles a block.
    #[error("output blocks are full")]
    Full,
    /// A submission exceeds 20 ms of source PCM.
    #[error("source packet exceeds 480 samples")]
    PacketTooLarge,
}

/// A notification from the device's error path, for the stream owner to handle.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    /// The device reported a discontinuity; refill without replacing the stream.
    Refill = 1,
    /// The backend rerouted the live stream.
    Rerouted = 2,
    /// The stream cannot continue; retire it before opening another.
    Lost = 3,
}

impl OutputEvent {
    #[expect(
        clippy::as_conversions,
        reason = "repr(u8) encodes notification priority"
    )]
    fn latch(pending: &AtomicU8, kind: ErrorKind) {
        let event = match kind {
            ErrorKind::Xrun => Self::Refill,
            ErrorKind::DeviceChanged => Self::Rerouted,
            _ => Self::Lost,
        };
        pending.fetch_max(event as u8, Ordering::Release);
    }

    fn take(pending: &AtomicU8) -> Option<Self> {
        match pending.swap(0, Ordering::AcqRel) {
            1 => Some(Self::Refill),
            2 => Some(Self::Rerouted),
            3 => Some(Self::Lost),
            _ => None,
        }
    }
}

/// One stream and its worker-side converter. Keep them on the owning audio worker.
pub struct Output {
    stream: cpal::Stream,
    submit: Submit,
    pending: Arc<AtomicU8>,
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("endpoint_rate", &self.submit.rate)
            .finish_non_exhaustive()
    }
}

impl Output {
    /// Create a paused float stream at the default endpoint's rate.
    ///
    /// # Errors
    /// The default is absent, has an unsupported format, or cannot open.
    pub fn open_default() -> Result<Self, OutputError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(OutputError::Unavailable)?;
        let mix = device
            .default_output_config()
            .map_err(|_| OutputError::Unavailable)?;
        let rate = mix.sample_rate();
        let channels = mix.channels();
        if !(MIN_RATE..=MAX_RATE).contains(&rate) || channels == 0 {
            return Err(OutputError::Unavailable);
        }
        let preferred = channels.min(2);
        match Self::open(&device, rate, preferred) {
            Err(ErrorKind::UnsupportedConfig) if (3..=MAX_CHANNELS).contains(&channels) => {
                Self::open(&device, rate, channels).map_err(|_| OutputError::Unavailable)
            }
            result => result.map_err(|_| OutputError::Unavailable),
        }
    }

    fn open(device: &cpal::Device, rate: u32, channels: u16) -> Result<Self, ErrorKind> {
        let (submit, mut playback) = pair(rate, usize::from(channels));
        let pending = Arc::new(AtomicU8::new(0));
        let notices = Arc::clone(&pending);
        let config = StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size: BufferSize::Default,
        };
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], _| playback.fill(out),
                move |error| OutputEvent::latch(&notices, error.kind()),
                None,
            )
            .map_err(|error| error.kind())?;
        Ok(Self {
            stream,
            submit,
            pending,
        })
    }

    /// Resume or start the callback.
    ///
    /// # Errors
    /// The device refused playback.
    pub fn play(&self) -> Result<(), OutputError> {
        self.stream.play().map_err(|_| OutputError::Unavailable)
    }

    /// Pause an idle callback, not an in-flight stop.
    ///
    /// # Errors
    /// The device refused to pause.
    pub fn pause(&self) -> Result<(), OutputError> {
        self.stream.pause().map_err(|_| OutputError::Unavailable)
    }

    /// Convert and enqueue up to 480 canonical mono samples; an end packet flushes the converter.
    ///
    /// # Errors
    /// An oversized packet or full pool leaves the input unconsumed. Retire the output if its
    /// block pool cannot continue.
    pub fn submit(&mut self, samples: &[i16], kind: PacketKind) -> Result<(), OutputError> {
        self.submit.packet(samples, kind)
    }

    /// Take the strongest pending notification. Loss outranks reroute, which outranks refill.
    pub fn take_event(&self) -> Option<OutputEvent> {
        OutputEvent::take(&self.pending)
    }
}

struct Buffers {
    free: ArrayQueue<Block>,
    ready: ArrayQueue<Block>,
}

struct Submit {
    buffers: Arc<Buffers>,
    rate: u32,
    channels: usize,
    converter: Option<Resampler>,
    mono: Zeroizing<Vec<f32>>,
}

struct Playback {
    buffers: Arc<Buffers>,
    current: Option<Block>,
    offset: usize,
}

fn pair(rate: u32, channels: usize) -> (Submit, Playback) {
    let buffers = Arc::new(Buffers {
        free: ArrayQueue::new(BLOCKS),
        ready: ArrayQueue::new(BLOCKS),
    });
    for _ in 0..BLOCKS {
        let block = Zeroizing::new(Vec::with_capacity(MAX_FRAMES * channels));
        assert!(buffers.free.push(block).is_ok(), "one allocation per slot");
    }
    let submit = Submit {
        buffers: Arc::clone(&buffers),
        rate,
        channels,
        converter: None,
        mono: Zeroizing::new(Vec::with_capacity(MAX_FRAMES)),
    };
    let playback = Playback {
        buffers,
        current: None,
        offset: 0,
    };
    (submit, playback)
}

impl Submit {
    fn packet(&mut self, samples: &[i16], kind: PacketKind) -> Result<(), OutputError> {
        if samples.len() > PACKET_SAMPLES {
            return Err(OutputError::PacketTooLarge);
        }
        if samples.is_empty() && (kind == PacketKind::More || self.converter.is_none()) {
            return Ok(());
        }
        let mut block = self.buffers.free.pop().ok_or(OutputError::Full)?;
        block.zeroize();
        let mut input = Zeroizing::new([0.0; PACKET_SAMPLES]);
        for (out, &sample) in input.iter_mut().zip(samples) {
            *out = f32::from(sample) / 32_768.0;
        }
        let input = &input[..samples.len()];
        if self.rate == SOURCE_RATE {
            self.mono.extend_from_slice(input);
        } else {
            let converter = self.converter.get_or_insert_with(|| {
                let mut converter =
                    Resampler::new(SOURCE_RATE, self.rate).expect("the endpoint rate is checked");
                converter.reserve_input(PACKET_SAMPLES);
                converter
            });
            converter.push(input, &mut self.mono);
            if kind == PacketKind::End {
                converter.finish(&mut self.mono);
                self.converter = None;
            }
        }
        for &sample in self.mono.iter() {
            let sample = sample.clamp(-1.0, 1.0);
            block.push(sample);
            if self.channels >= 2 {
                block.push(sample);
                let end = block.len() + self.channels - 2;
                block.resize(end, 0.0);
            }
        }
        self.mono.zeroize();
        let queue = if block.is_empty() {
            &self.buffers.free
        } else {
            &self.buffers.ready
        };
        queue.push(block).map_err(|_| OutputError::Unavailable)
    }
}

impl Playback {
    fn fill(&mut self, mut out: &mut [f32]) {
        while !out.is_empty() {
            if self.current.is_none() {
                self.current = self.buffers.ready.pop();
                self.offset = 0;
            }
            let Some(block) = self.current.take() else {
                break;
            };
            let len = out.len().min(block.len() - self.offset);
            let (head, rest) = out.split_at_mut(len);
            head.copy_from_slice(&block[self.offset..self.offset + len]);
            self.offset += len;
            out = rest;
            if self.offset != block.len() {
                self.current = Some(block);
            } else if let Err(block) = self.buffers.free.push(block) {
                self.current = Some(block);
                break;
            }
        }
        out.fill(0.0);
    }
}
