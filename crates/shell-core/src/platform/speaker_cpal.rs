//! Dormant output seam for the speech worker. Nothing in the normal link opens it.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{BufferSize, ErrorKind, StreamConfig};
use kumochi_audio::Resampler;
use kumochi_protocol::message::{MAX_RATE, MIN_RATE};
use thiserror::Error;
use zeroize::{Zeroize as _, Zeroizing};

mod control;
#[expect(
    unsafe_code,
    reason = "fixed SPSC slots transfer ownership by atomic publication"
)]
mod handoff;
mod lifetime;
mod source;

#[cfg(test)]
mod tests;

pub(crate) use control::{Control, TailAck};
use control::{CopyDenied, frame_time};
use handoff::{Consumer, Producer};
use lifetime::{OwnedStream, Owner, Permit, Shared, cleanup_sender};
use source::{Conversion, Stamp};
pub(crate) use source::{Presented, Source};

const SOURCE_RATE: u32 = 24_000;
const PACKET_SAMPLES: usize = 480;
const MAX_FRAMES: usize = 4_096;
const MAX_CHANNELS: u16 = 32;
const BLOCKS: usize = 8;
struct Block {
    samples: Vec<f32>,
}

impl Block {
    fn clear(&mut self) {
        if !self.samples.is_empty() {
            self.samples.zeroize();
            Self::note_clear(self);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn note_clear(block: &Self) {
        let _ = CLEAR_OBSERVER.try_with(|observer| {
            if let Some(observe) = observer.get() {
                observe(block.samples.as_ptr().addr());
            }
        });
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn note_clear(_: &Self) {}
}

#[cfg(any(test, feature = "test-support"))]
type ClearObserver = std::cell::Cell<Option<fn(usize)>>;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static CLEAR_OBSERVER: ClearObserver = const { std::cell::Cell::new(None) };
}

impl Drop for Block {
    fn drop(&mut self) {
        self.clear();
    }
}

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
/// Cancellation silences any later callback; backend closure destruction acknowledges retirement.
pub struct Output(OwnedStream<BackendStream>);

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("endpoint_rate", &self.0.owner.submit().rate)
            .finish_non_exhaustive()
    }
}

impl Output {
    /// Create a paused float stream at the default endpoint's rate.
    ///
    /// # Errors
    /// The default is absent, has an unsupported format, or cannot open.
    pub fn open_default() -> Result<Self, OutputError> {
        let (stream, Transport(owner)) =
            BackendStream::open(&Arc::new(Control::default()), Instant::now())?;
        Ok(Self(OwnedStream { stream, owner }))
    }

    /// Resume or start the callback.
    ///
    /// # Errors
    /// The device refused playback.
    pub fn play(&self) -> Result<(), OutputError> {
        self.0.stream.play()
    }

    /// Pause an idle callback, not an in-flight stop.
    ///
    /// # Errors
    /// The device refused to pause.
    pub fn pause(&self) -> Result<(), OutputError> {
        self.0.stream.pause()
    }

    /// Convert and enqueue up to 480 canonical mono samples; an end packet flushes the converter.
    ///
    /// # Errors
    /// An oversized packet or full pool leaves the input unconsumed. Retire the output if its
    /// block pool cannot continue.
    pub fn submit(&mut self, samples: &[i16], kind: PacketKind) -> Result<(), OutputError> {
        self.0.owner.submit_mut().packet(samples, kind)
    }

    /// Take the strongest pending notification. Loss outranks reroute, which outranks refill.
    pub fn take_event(&self) -> Option<OutputEvent> {
        OutputEvent::take(&self.0.owner.submit().shared.pending)
    }
}

pub(crate) struct BackendStream<S = cpal::Stream>(S, Arc<AtomicBool>);

impl<S> Drop for BackendStream<S> {
    fn drop(&mut self) {
        self.1.store(true, Ordering::Release);
    }
}

impl BackendStream {
    pub(crate) fn open(
        control: &Arc<Control>,
        origin: Instant,
    ) -> Result<(Self, Transport), OutputError> {
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
        match Self::build(&device, rate, preferred, control, origin) {
            Err(ErrorKind::UnsupportedConfig) if (3..=MAX_CHANNELS).contains(&channels) => {
                Self::build(&device, rate, channels, control, origin)
                    .map_err(|_| OutputError::Unavailable)
            }
            result => result.map_err(|_| OutputError::Unavailable),
        }
    }

    fn build(
        device: &cpal::Device,
        rate: u32,
        channels: u16,
        control: &Arc<Control>,
        origin: Instant,
    ) -> Result<(Self, Transport), ErrorKind> {
        static OCCUPIED: AtomicBool = AtomicBool::new(false);
        let permit = Permit::claim(&OCCUPIED).ok_or(ErrorKind::DeviceBusy)?;
        let cleanup = cleanup_sender().ok_or(ErrorKind::ResourceExhausted)?;
        let (submit, mut playback) = pair(rate, usize::from(channels), Arc::clone(control));
        let cancelled = Arc::clone(&submit.shared.cancelled);
        let notices = Arc::clone(&submit.shared);
        let signal = Arc::clone(control);
        let owner = Owner::new(submit, permit, cleanup);
        let config = StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size: BufferSize::Default,
        };
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], info| {
                    let start = playback.control.time(info.timestamp().playback);
                    playback.fill_at(out, start);
                },
                move |error| {
                    OutputEvent::latch(&notices.pending, error.kind());
                    signal.wake();
                },
                Some(Duration::from_millis(250)),
            )
            .map_err(|error| error.kind())?;
        control.calibrate(stream.now(), origin.elapsed());
        Ok((Self(stream, cancelled), Transport(owner)))
    }

    pub(crate) fn play(&self) -> Result<(), OutputError> {
        self.0.play().map_err(|_| OutputError::Unavailable)
    }

    pub(crate) fn pause(&self) -> Result<(), OutputError> {
        self.0.pause().map_err(|_| OutputError::Unavailable)
    }
}

pub(crate) struct Transport(Owner);

#[expect(
    dead_code,
    reason = "transport is passed to the queued owner in a separate change"
)]
impl Transport {
    pub(crate) fn control(&self) -> &Control {
        &self.0.submit().control
    }

    pub(crate) fn submit(
        &mut self,
        samples: &[i16],
        kind: PacketKind,
        source: Source,
    ) -> Result<(), OutputError> {
        self.0.submit_mut().packet_for(samples, kind, Some(source))
    }

    pub(crate) fn presentation(&mut self) -> Option<Presented> {
        self.0.submit_mut().observed.pop()
    }

    pub(crate) fn take_event(&self) -> Option<OutputEvent> {
        OutputEvent::take(&self.0.submit().shared.pending)
    }
}

struct ReadyBlock {
    index: usize,
    pcm: Arc<Block>,
    stamp: Option<Stamp>,
}

struct Submit {
    free: Consumer<usize>,
    ready: Producer<ReadyBlock>,
    observed: Consumer<Presented>,
    conversion: Option<Conversion>,
    blocks: [Arc<Block>; BLOCKS],
    spare: Option<usize>,
    rate: u32,
    channels: usize,
    converter: Option<Resampler>,
    mono: Zeroizing<Vec<f32>>,
    shared: Arc<Shared>,
    control: Arc<Control>,
}

/// Dropping the last field releases the callback witness after its rings and block.
struct Playback {
    free: Producer<usize>,
    ready: Consumer<ReadyBlock>,
    observed: Producer<Presented>,
    current: Option<ReadyBlock>,
    recycle: Option<usize>,
    offset: usize,
    channels: usize,
    rate: u32,
    ramp: usize,
    last: f32,
    last_sound: Option<Duration>,
    uncertain: bool,
    control: Arc<Control>,
    shared: Arc<Shared>,
}

fn pair(rate: u32, channels: usize, control: Arc<Control>) -> (Submit, Playback) {
    let shared = Arc::new(Shared::default());
    let (mut recycle, free) = handoff::pair();
    let (ready, render) = handoff::pair();
    let (record, observed) = handoff::pair();
    let blocks = std::array::from_fn(|index| {
        assert!(recycle.push(index).is_ok(), "one allocation per slot");
        Arc::new(Block {
            samples: Vec::with_capacity(MAX_FRAMES * channels),
        })
    });
    let submit = Submit {
        free,
        ready,
        observed,
        conversion: None,
        blocks,
        spare: None,
        rate,
        channels,
        converter: None,
        mono: Zeroizing::new(Vec::with_capacity(MAX_FRAMES)),
        shared: Arc::clone(&shared),
        control: Arc::clone(&control),
    };
    let playback = Playback {
        free: recycle,
        ready: render,
        observed: record,
        current: None,
        recycle: None,
        offset: 0,
        channels,
        rate,
        ramp: 0,
        last: 0.0,
        last_sound: None,
        uncertain: false,
        control,
        shared,
    };
    (submit, playback)
}

impl Submit {
    fn clear_available(&mut self) {
        self.converter = None;
        if !self.mono.is_empty() {
            self.mono.zeroize();
        }
        for block in &mut self.blocks {
            if let Some(block) = Arc::get_mut(block) {
                block.clear();
            }
        }
    }

    fn packet(&mut self, samples: &[i16], kind: PacketKind) -> Result<(), OutputError> {
        self.packet_for(samples, kind, None)
    }

    fn packet_for(
        &mut self,
        samples: &[i16],
        kind: PacketKind,
        source: Option<Source>,
    ) -> Result<(), OutputError> {
        if samples.len() > PACKET_SAMPLES {
            return Err(OutputError::PacketTooLarge);
        }
        if samples.is_empty() && (kind == PacketKind::More || self.converter.is_none()) {
            if kind == PacketKind::End {
                self.conversion = None;
            }
            return Ok(());
        }
        let index = self
            .spare
            .take()
            .or_else(|| self.free.pop())
            .ok_or(OutputError::Full)?;
        let block = Arc::get_mut(&mut self.blocks[index]).ok_or(OutputError::Unavailable)?;
        block.clear();
        let block = &mut block.samples;
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
        let stamp = source.map(|source| {
            self.conversion
                .get_or_insert_with(|| Conversion::start(source.at))
                .stamp(source, samples.len(), self.mono.len())
        });
        if kind == PacketKind::End {
            self.conversion = None;
        }
        self.mono.zeroize();
        if block.is_empty() {
            self.spare = Some(index);
            return Ok(());
        }
        let ready = ReadyBlock {
            index,
            pcm: Arc::clone(&self.blocks[index]),
            stamp,
        };
        self.ready.push(ready).map_err(|_| OutputError::Unavailable)
    }
}

impl Playback {
    fn fill_at(&mut self, out: &mut [f32], start: Option<Duration>) {
        self.render(out, start, Control::begin_copy, || {});
    }

    fn render(
        &mut self,
        mut out: &mut [f32],
        start: Option<Duration>,
        mut admit: impl FnMut(&Control) -> Result<(), CopyDenied>,
        mut after_copy: impl FnMut(),
    ) {
        let total = out.len();
        let mut report: Option<Presented> = None;
        let mut worked = false;
        let limit = usize::try_from(self.rate / 400).expect("bounded output rate") * self.channels;
        let interruption = loop {
            if self.shared.cancelled.load(Ordering::Acquire) {
                break Some(CopyDenied::Silence);
            }
            if let Some(reason) = self.control.interruption() {
                break Some(reason);
            }
            if !self.return_slot() || out.is_empty() {
                break None;
            }
            let at = (total - out.len()) / self.channels;
            if self.current.is_none() {
                self.current = self.ready.pop();
                self.offset = 0;
            }
            let Some(block) = self.current.as_ref() else {
                break None;
            };
            let len = out
                .len()
                .min(block.pcm.samples.len() - self.offset)
                .min(limit);
            let len = len - len % self.channels;
            if len == 0 {
                break None;
            }
            if let Err(reason) = admit(&self.control) {
                break Some(reason);
            }
            let (head, rest) = out.split_at_mut(len);
            head.copy_from_slice(&block.pcm.samples[self.offset..self.offset + len]);
            let copied = block.stamp.map(|stamp| {
                stamp.copied(
                    self.offset / self.channels,
                    len / self.channels,
                    self.rate,
                    start,
                    at,
                )
            });
            self.offset += len;
            if self.offset == block.pcm.samples.len() {
                self.recycle = Some(block.index);
                self.current = None;
            }
            out = rest;
            self.last = head
                .chunks_exact(self.channels)
                .last()
                .map_or(0.0, |frame| frame[0]);
            self.remember(head, frame_time(start, at, self.rate));
            if let Some((handed, span)) = copied {
                self.control.handed_through(handed);
                if let Some(span) = span {
                    if let Some(previous) = &mut report
                        && previous.source.segment == span.source.segment
                        && previous.through == span.first
                    {
                        previous.through = span.through;
                        previous.at = previous.at.and(span.at);
                    } else {
                        self.publish(report.replace(span));
                    }
                }
            }
            self.control.end_copy();
            worked = true;
            after_copy();
        };
        self.publish(report);
        if interruption == Some(CopyDenied::Stop) && !self.shared.cancelled.load(Ordering::Acquire)
        {
            let at = (total - out.len()) / self.channels;
            self.discard_ready();
            worked |= self.control.tail() == TailAck::Pending;
            self.fade(out, frame_time(start, at, self.rate));
        } else if !out.is_empty() {
            out.fill(0.0);
            self.last = 0.0;
        }
        if worked {
            self.control.wake();
        }
    }

    fn publish(&mut self, report: Option<Presented>) {
        if let Some(report) = report
            && self.observed.push(report).is_err()
        {
            self.control.overflow();
        }
    }

    fn return_slot(&mut self) -> bool {
        if let Some(index) = self.recycle.take()
            && let Err(index) = self.free.push(index)
        {
            self.recycle = Some(index);
            return false;
        }
        true
    }

    fn discard_ready(&mut self) {
        for _ in 0..=BLOCKS {
            if !self.return_slot() {
                break;
            }
            let Some(block) = self.current.take().or_else(|| self.ready.pop()) else {
                break;
            };
            self.recycle = Some(block.index);
        }
    }

    fn fade(&mut self, out: &mut [f32], start: Option<Duration>) {
        out.fill(0.0);
        if self.control.tail() != TailAck::Pending {
            return;
        }
        let max = self.rate / 200;
        if self.ramp == 0 && self.last.abs() > 0.0 {
            self.ramp = usize::try_from(max).expect("bounded ramp");
        }
        for frame in out.chunks_exact_mut(self.channels).take(self.ramp) {
            self.ramp -= 1;
            let remaining = u32::try_from(self.ramp).expect("bounded ramp");
            let gain = f64::from(remaining) / f64::from(max);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::as_conversions,
                reason = "gain is in 0..1"
            )]
            let sample = self.last * gain as f32;
            frame[..self.channels.min(2)].fill(sample);
        }
        self.remember(out, start);
        if self.ramp == 0 {
            self.last = 0.0;
            self.control.latch(if self.uncertain {
                None
            } else {
                self.last_sound.or(start)
            });
        }
    }

    fn remember(&mut self, out: &[f32], start: Option<Duration>) {
        if let Some(last) = out
            .chunks_exact(self.channels)
            .rposition(|frame| frame[0].abs() > 0.0)
        {
            let at = frame_time(start, last, self.rate);
            self.uncertain |= at.is_none();
            self.last_sound = self.last_sound.max(at);
        }
    }
}
