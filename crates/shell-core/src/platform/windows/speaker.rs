//! The Windows speaker: one event-driven, shared-mode WASAPI render stream on the default
//! output, read through the same stream's `IAudioClock`. This file only maps platform results
//! and owns the thread and its waits; every cursor, ledger and quiet rule is the shared
//! [`Engine`]'s (docs/development/speaker.md).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK, WAIT_FAILED};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioClient, IAudioClock, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCWSTR};
use zeroize::Zeroize;

use super::microphone::{Apartment, Mix, read_format};
use crate::platform::microphone::Render;
use crate::speaker::{
    Device, Engine, Fault, Observation, Outlet, Release, Speaker, Staged, Worker, launch, serve,
};

/// The longest wait for the device's event; each pass also takes commands and data.
const WAIT_MS: u32 = 20;
/// How long opening may take before the link goes on without output [guess].
const OPEN: Duration = Duration::from_secs(2);
/// WASAPI's periods and clock times are in 100 ns units.
const HNS: u64 = 10_000_000;

/// Set while a speaker thread runs, so a thread still tearing down never overlaps a new stream.
static LIVE: AtomicBool = AtomicBool::new(false);

/// Clears [`LIVE`] when its thread ends.
struct Live;

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.store(false, Ordering::Release);
    }
}

/// Opens the default output for a connection, blocking the caller (the link thread, never the
/// UI) until the stream runs, fails or takes longer than [`OPEN`]; a refused speaker then takes
/// the usual bounded teardown to let go. `None` when there's no
/// usable output, another speaker's thread hasn't ended yet, or the stream started too late to
/// be accepted (see [`launch`]).
pub fn open(outlet: Outlet) -> Option<Speaker> {
    if LIVE.swap(true, Ordering::AcqRel) {
        return None;
    }
    let live = Live;
    launch(outlet, OPEN, move |worker, ready| {
        let _live = live;
        run(worker, &ready);
    })
}

fn run(worker: Worker, ready: &SyncSender<bool>) {
    let Ok(_apartment) = Apartment::new() else {
        let _told = ready.send(false);
        return;
    };
    let Some(mut stream) = Stream::open() else {
        let _told = ready.send(false);
        return;
    };
    let mut engine = Engine::new(
        stream.rate,
        worker.generation(),
        stream.qpc_frequency,
        worker.revocation(),
        worker.tail(),
    );
    let started = engine.begin(stream.frequency, stream.origin).is_ok()
        && stream.service(&mut engine).is_ok()
        && stream.start();
    let _told = ready.send(started);
    if started {
        serve(worker, engine, stream);
    }
}

/// An auto-reset event the stream signals each period.
struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this handle came from CreateEventW and is closed once, after the client let go.
        let _closed = unsafe { CloseHandle(self.0) };
    }
}

/// The stream, in drop order: the client and its services go before the event they signal.
struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: Event,
    writer: Render,
    rate: u32,
    buffer: u32,
    lead: u32,
    frequency: u64,
    origin: u64,
    qpc_frequency: u64,
    opened: Instant,
    /// Mono samples staged for a write.
    scratch: Vec<i16>,
    /// Those samples as the device's frames, copied into its buffer.
    bytes: Vec<u8>,
    started: bool,
}

impl Stream {
    fn open() -> Option<Self> {
        // Declared first so it's closed last, after any client registered with it.
        // SAFETY: an unnamed auto-reset event; Event closes it.
        let event = Event(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.ok()?);
        // SAFETY: COM is initialized on this thread; the documented class implements this interface.
        let devices: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
        // SAFETY: eRender chooses an output endpoint, which this stream keeps for its life.
        let device = unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
        // SAFETY: this endpoint supplies IAudioClient; no activation parameters are needed.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.ok()?;
        // SAFETY: client is live; Mix frees the allocation on every path.
        let mix = Mix(unsafe { client.GetMixFormat() }.ok()?);
        let format = read_format(&mix).ok()?;
        let writer = format.render()?;
        // SAFETY: the validated mix format is live; event-driven shared mode at the engine's own
        // buffer and period, with no rate conversion and no loopback.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                mix.0,
                None,
            )
        }
        .ok()?;
        // SAFETY: the event was declared before the client, so it outlives it on every path.
        unsafe { client.SetEventHandle(event.0) }.ok()?;
        // SAFETY: the client is initialized.
        let buffer = unsafe { client.GetBufferSize() }.ok()?;
        let mut period = 0;
        // SAFETY: the output outlives the call.
        unsafe { client.GetDevicePeriod(Some(&raw mut period), None) }.ok()?;
        // SAFETY: the initialized render client supplies this service.
        let render: IAudioRenderClient = unsafe { client.GetService() }.ok()?;
        // SAFETY: the same client supplies its clock.
        let clock: IAudioClock = unsafe { client.GetService() }.ok()?;
        // SAFETY: the clock is live.
        let frequency = unsafe { clock.GetFrequency() }.ok()?;
        let mut qpc_frequency = 0;
        // SAFETY: the output outlives the call.
        unsafe { QueryPerformanceFrequency(&raw mut qpc_frequency) }.ok()?;
        // Everything that can fail is settled before the owners move into the stream, so its
        // field order, client before event, holds on every exit.
        let rate = format.rate;
        let period = u64::try_from(period).ok()?.checked_mul(u64::from(rate))? / HNS;
        if buffer == 0 || buffer > rate / 10 {
            return None;
        }
        let lead = buffer.min(u32::try_from(period.checked_mul(2)?).ok()?.max(1));
        let qpc_frequency = u64::try_from(qpc_frequency)
            .ok()
            .filter(|&ticks| ticks > 0)?;
        let frames = usize::try_from(buffer).ok()?;
        let scratch = vec![0; frames];
        let bytes = vec![0; frames.checked_mul(writer.frame())?];
        let mut stream = Self {
            client,
            render,
            clock,
            event,
            writer,
            rate,
            buffer,
            lead,
            frequency,
            origin: 0,
            qpc_frequency,
            opened: Instant::now(),
            scratch,
            bytes,
            started: false,
        };
        let origin = stream.position().ok()?;
        if !origin.precise {
            return None;
        }
        stream.origin = origin.position;
        Some(stream)
    }

    fn start(&mut self) -> bool {
        // SAFETY: the client is initialized and holds the first frames; only this thread runs it.
        self.started = unsafe { self.client.Start() }.is_ok();
        self.started
    }
}

impl Device for Stream {
    /// Waits for the device's event, at most [`WAIT_MS`].
    fn wait(&self) -> Result<(), Fault> {
        // SAFETY: the event is open for the stream's life.
        if unsafe { WaitForSingleObject(self.event.0, WAIT_MS) } == WAIT_FAILED {
            return Err(Fault::Lost);
        }
        Ok(())
    }

    /// Reads the device's clock against what's written so far, then tops it up to the lead.
    fn service(&mut self, engine: &mut Engine) -> Result<(), Fault> {
        let padding = self.padding()?;
        engine.observe(self.position()?)?;
        let frames = self
            .lead
            .saturating_sub(padding)
            .min(self.buffer.saturating_sub(padding));
        if frames == 0 {
            return Ok(());
        }
        let n = usize::try_from(frames).map_err(|_| Fault::Lost)?;
        let bytes = n * self.writer.frame();
        let staged = engine.stage(&mut self.scratch[..n], padding)?;
        self.writer
            .write(&self.scratch[..n], &mut self.bytes[..bytes]);
        self.scratch[..n].zeroize();
        let released = self.copy_out(frames, bytes, staged);
        self.bytes[..bytes].zeroize();
        released
    }

    fn stop(&mut self) -> bool {
        // SAFETY: only this thread runs the stream, and no buffer is held.
        !std::mem::take(&mut self.started) || unsafe { self.client.Stop() }.is_ok()
    }
}

impl Stream {
    /// Copies the staged frames into the device's buffer and gives it back as
    /// [`Staged::release`] decides: as written, then committed with the padding read after it,
    /// or as silence once the speaker was revoked.
    fn copy_out(&self, frames: u32, bytes: usize, staged: Staged<'_>) -> Result<(), Fault> {
        // SAFETY: frames fit the free space just measured; only this thread writes the stream.
        let data = match unsafe { self.render.GetBuffer(frames) } {
            Ok(data) if !data.is_null() => data,
            _ => {
                staged.abandon();
                return Err(Fault::Lost);
            }
        };
        // SAFETY: WASAPI lends frames * block_align writable bytes until ReleaseBuffer; the source
        // is initialized, holds exactly that many bytes and is our own, so they can't overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.bytes.as_ptr(), data, bytes) };
        staged.release(
            |release| {
                let flags = match release {
                    Release::Content => 0,
                    Release::Silent => AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned(),
                };
                // SAFETY: balances the GetBuffer above; the lent bytes aren't touched after this.
                unsafe { self.render.ReleaseBuffer(frames, flags) }.map_err(|_| Fault::Lost)?;
                Ok(self.opened.elapsed())
            },
            || self.padding().ok(),
        )
    }

    /// Frames written and not yet read by the device.
    fn padding(&self) -> Result<u32, Fault> {
        // SAFETY: no buffer is held; this is a padding query.
        unsafe { self.client.GetCurrentPadding() }.map_err(|_| Fault::Lost)
    }

    /// The clock's position with its 100 ns QPC time. `S_FALSE`, which the wrapper turns into
    /// success, marks the reading imprecise.
    fn position(&self) -> Result<Observation, Fault> {
        let (mut position, mut host_time) = (0, 0);
        let vtable = self.clock.vtable();
        // SAFETY: outputs outlive the call; the raw HRESULT keeps S_FALSE apart from S_OK.
        let result = unsafe {
            (vtable.GetPosition)(self.clock.as_raw(), &raw mut position, &raw mut host_time)
        };
        if result.is_err() {
            return Err(Fault::Lost);
        }
        Ok(Observation {
            position,
            frequency: self.frequency,
            host_time,
            host_frequency: HNS,
            precise: result == S_OK,
            service: self.opened.elapsed(),
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.scratch.zeroize();
        self.bytes.zeroize();
        // SAFETY: only this thread runs the stream, and no buffer is held.
        if self.started && unsafe { self.client.Stop() }.is_err() {
            tracing::warn!("the speaker's stream didn't stop cleanly");
        }
    }
}
