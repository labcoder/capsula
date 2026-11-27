//! Input-only WASAPI polling worker. No callbacks, loopback, resampling or reopen.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use async_channel::Sender;
use kumochi_protocol::message::{MAX_MIC_SAMPLES, MicPcm};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eCommunications,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{GUID, HRESULT, Interface};
use zeroize::{Zeroize, Zeroizing};

use crate::platform::microphone::{
    CAPTURE_BUFFER, Finalization, Format, Generation, MicError, Mode, POLL, PacketBoundary,
    PacketStatus, Packetizer, Running, Sample, copy_batch,
};

pub(super) struct Apartment;
impl Apartment {
    pub(super) fn new() -> Result<Self, MicError> {
        // SAFETY: this dedicated worker owns its COM apartment until all interfaces drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|_| MicError::Unavailable)?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances this thread's successful CoInitializeEx, after interface teardown.
        unsafe { CoUninitialize() };
    }
}

pub(super) struct Mix(pub(super) *mut WAVEFORMATEX);
impl Drop for Mix {
    fn drop(&mut self) {
        // SAFETY: GetMixFormat allocated this pointer with the COM allocator.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

struct Source {
    device: IMMDevice,
    client: IAudioClient,
    input: IAudioCaptureClient,
    format: Format,
    started: bool,
}

impl Source {
    fn open(token: &Generation, session: &AtomicBool) -> Result<Self, MicError> {
        live(token, session)?;
        // SAFETY: COM is initialized on this thread; the documented class implements this interface.
        let devices: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|_| MicError::Unavailable)?;
        live(token, session)?;
        // SAFETY: eCapture chooses input only; this endpoint is retained for the entire generation.
        let device = unsafe { devices.GetDefaultAudioEndpoint(eCapture, eCommunications) }
            .map_err(|_| MicError::Unavailable)?;
        live(token, session)?;
        // SAFETY: this endpoint supplies IAudioClient; no activation parameters are needed.
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|_| MicError::Unavailable)?;
        live(token, session)?;
        // SAFETY: client is live; Mix frees the returned allocation on every subsequent path.
        let mix = Mix(unsafe { client.GetMixFormat() }.map_err(|_| MicError::Format)?);
        let format = read_format(&mix)?;
        live(token, session)?;
        let buffer =
            i64::try_from(CAPTURE_BUFFER.as_nanos() / 100).map_err(|_| MicError::Format)?;
        // SAFETY: the mix format is live and validated. Flags exclude loopback and rate conversion.
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, buffer, 0, mix.0, None) }
            .map_err(|_| MicError::Unavailable)?;
        live(token, session)?;
        // SAFETY: the initialized input client supplies this capture service.
        let input = unsafe { client.GetService() }.map_err(|_| MicError::Unavailable)?;
        Ok(Self {
            device,
            client,
            input,
            format,
            started: false,
        })
    }

    fn check(&self) -> Result<(), MicError> {
        // SAFETY: the pinned endpoint remains owned on this COM thread.
        if unsafe { self.device.GetState() }.map_err(|_| MicError::Unavailable)?
            != DEVICE_STATE_ACTIVE
        {
            return Err(MicError::Unavailable);
        }
        // SAFETY: this off-pump query owns its COM allocation until Mix drops.
        let mix = Mix(unsafe { self.client.GetMixFormat() }.map_err(|_| MicError::Format)?);
        if read_format(&mix)? != self.format {
            return Err(MicError::Format);
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), MicError> {
        if self.started {
            self.started = false;
            // SAFETY: only this worker starts/stops this client, and no native buffer is borrowed.
            unsafe { self.client.Stop() }.map_err(|error| teardown_error(error.code()))?;
        }
        Ok(())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if self.stop().is_err() {
            // The explicit stop path returns Unconfirmed; unwinding never creates a receipt.
        }
    }
}

/// A device gone mid-hold ended its stream, and dropping the client releases it; any other
/// teardown failure leaves the stream's state unknown.
pub(in crate::platform) fn teardown_error(code: HRESULT) -> MicError {
    if code == AUDCLNT_E_DEVICE_INVALIDATED {
        MicError::Unavailable
    } else {
        MicError::Unconfirmed
    }
}

pub(super) fn read_format(mix: &Mix) -> Result<Format, MicError> {
    read_mix(mix)?.0.check()
}

pub(super) fn read_mix(mix: &Mix) -> Result<(Format, u32), MicError> {
    if mix.0.is_null() {
        return Err(MicError::Format);
    }
    // SAFETY: GetMixFormat returns at least WAVEFORMATEX; read_unaligned handles packed layout.
    let wave = unsafe { mix.0.read_unaligned() };
    let (tag, valid, mask) = if wave.wFormatTag == 0xfffe {
        if usize::from(wave.cbSize) < size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()
        {
            return Err(MicError::Format);
        }
        // SAFETY: cbSize establishes that this allocation contains WAVEFORMATEXTENSIBLE.
        let ext = unsafe { mix.0.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        // SAFETY: for PCM/float extensible formats this union field is the valid-bit count.
        let valid = unsafe { ext.Samples.wValidBitsPerSample };
        let subtype = ext.SubFormat;
        let tag = if subtype == GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71) {
            1
        } else if subtype == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71) {
            3
        } else {
            return Err(MicError::Format);
        };
        (tag, valid, ext.dwChannelMask)
    } else {
        (wave.wFormatTag, wave.wBitsPerSample, 0)
    };
    if valid != wave.wBitsPerSample {
        return Err(MicError::Format);
    }
    let sample = match (tag, wave.wBitsPerSample) {
        (1, 8) => Sample::U8,
        (1, 16) => Sample::I16,
        (1, 24) => Sample::I24,
        (1, 32) => Sample::I32,
        (3, 32) => Sample::F32,
        _ => return Err(MicError::Format),
    };
    let format = Format {
        rate: wave.nSamplesPerSec,
        channels: wave.nChannels,
        sample,
    };
    let align = format.bytes() * usize::from(format.channels);
    if usize::from(wave.nBlockAlign) != align
        || u64::from(wave.nAvgBytesPerSec)
            != u64::from(format.rate) * u64::try_from(align).map_err(|_| MicError::Format)?
    {
        return Err(MicError::Format);
    }
    Ok((format, mask))
}

fn live(token: &Generation, session: &AtomicBool) -> Result<(), MicError> {
    match token.mode(session, Instant::now())? {
        Mode::Release => Err(MicError::Cancelled),
        Mode::Preparing | Mode::Collecting => Ok(()),
    }
}

pub(in crate::platform) fn collection_start() -> Result<u64, MicError> {
    let mut frequency = 0;
    // SAFETY: the output is writable and stays alive through the call.
    unsafe { QueryPerformanceFrequency(&raw mut frequency) }.map_err(|_| MicError::Timestamp)?;
    let mut ticks = 0;
    // SAFETY: the output is writable and stays alive through the call.
    unsafe { QueryPerformanceCounter(&raw mut ticks) }.map_err(|_| MicError::Timestamp)?;
    capture_cutoff_100ns(ticks, frequency)
}

fn capture_cutoff_100ns(ticks: i64, frequency: i64) -> Result<u64, MicError> {
    let frequency = u64::try_from(frequency).map_err(|_| MicError::Timestamp)?;
    let ticks = u64::try_from(ticks).map_err(|_| MicError::Timestamp)?;
    if frequency == 0 || ticks == 0 {
        return Err(MicError::Timestamp);
    }
    // A cross-thread reading one tick ahead is ambiguous even if WASAPI rounds it up.
    let last_ambiguous = ((u128::from(ticks) + 1) * 10_000_000).div_ceil(u128::from(frequency));
    u64::try_from(last_ambiguous + 1).map_err(|_| MicError::Timestamp)
}

pub(crate) fn capture(
    token: &Generation,
    session: &AtomicBool,
    running: &Sender<Running>,
    output: &Sender<MicPcm>,
) -> Result<Finalization, MicError> {
    let mut boundary = PacketBoundary::default();
    let result = capture_with(token, session, running, output, &mut boundary);
    boundary.log();
    result
}

fn capture_with(
    token: &Generation,
    session: &AtomicBool,
    running: &Sender<Running>,
    output: &Sender<MicPcm>,
    boundary: &mut PacketBoundary,
) -> Result<Finalization, MicError> {
    let _apartment = match Apartment::new() {
        Ok(apartment) => apartment,
        Err(error) => return Ok(Finalization::Discarded(error)),
    };
    let mut source = match Source::open(token, session) {
        Ok(source) => source,
        Err(error) => {
            return Ok(match (error, token.mode(session, Instant::now())) {
                (MicError::Cancelled | MicError::Stale, Ok(Mode::Release)) => {
                    Finalization::Released { packets: 0 }
                }
                _ => Finalization::Discarded(error),
            });
        }
    };
    let mut packets = Packetizer::new(token.gesture(), source.format)?;
    let mut result = collect(
        &mut source,
        token,
        session,
        running,
        output,
        &mut packets,
        boundary,
    );
    boundary.queued(packets.high_water());
    if matches!(result, Err(MicError::Stale | MicError::Cancelled))
        && token.mode(session, Instant::now()) == Ok(Mode::Release)
    {
        result = Ok(());
    }
    if result.is_err() {
        token.cancel();
        packets.clear();
    }
    let stopped = source.stop();
    if stopped == Err(MicError::Unconfirmed) {
        return Err(MicError::Unconfirmed);
    }
    result = boundary.finish(result.and(stopped).and_then(|()| source.check()));
    if result.is_err() {
        token.cancel();
        packets.clear();
    }
    drop(source);
    if result == Err(MicError::Unconfirmed) {
        return Err(MicError::Unconfirmed);
    }
    match result {
        Ok(()) => match token.mode(session, Instant::now()) {
            Ok(Mode::Release) => {
                let finished = packets.finish(output);
                boundary.queued(packets.high_water());
                Ok(match finished {
                    Ok(packets) => Finalization::Released { packets },
                    Err(error) => {
                        token.cancel();
                        Finalization::Discarded(error)
                    }
                })
            }
            Err(error) => Ok(Finalization::Discarded(error)),
            _ => Ok(Finalization::Discarded(MicError::Cancelled)),
        },
        Err(error) => Ok(Finalization::Discarded(error)),
    }
}

fn collect(
    source: &mut Source,
    token: &Generation,
    session: &AtomicBool,
    running: &Sender<Running>,
    output: &Sender<MicPcm>,
    packets: &mut Packetizer,
    boundary: &mut PacketBoundary,
) -> Result<(), MicError> {
    live(token, session)?;
    source.check()?;
    let mut raw = Zeroizing::new(vec![
        0_u8;
        MAX_MIC_SAMPLES
            * usize::from(source.format.channels)
            * source.format.bytes()
    ]);
    live(token, session)?;
    boundary.started(Instant::now());
    source.started = true;
    // SAFETY: this initialized capture client has one worker, no borrowed buffer and no callbacks.
    unsafe { source.client.Start() }.map_err(|_| MicError::Unavailable)?;
    live(token, session)?;
    token.running()?;
    running
        .try_send(Running {
            gesture: token.gesture(),
            rate: source.format.rate,
        })
        .map_err(|_| MicError::Unavailable)?;
    let result = read(source, token, session, output, packets, &mut raw, boundary);
    if result.is_err() {
        token.cancel();
    }
    raw.zeroize();
    result
}

fn read(
    source: &Source,
    token: &Generation,
    session: &AtomicBool,
    output: &Sender<MicPcm>,
    packets: &mut Packetizer,
    raw: &mut [u8],
    boundary: &mut PacketBoundary,
) -> Result<(), MicError> {
    loop {
        let now = Instant::now();
        let mode = token.mode(session, now)?;
        boundary.observe(mode);
        if mode == Mode::Release {
            return Ok(());
        }
        // SAFETY: no buffer is currently held; this is a nonblocking capture-size query.
        if unsafe { source.input.GetNextPacketSize() }.map_err(|_| MicError::Unavailable)? == 0 {
            std::thread::park_timeout(POLL);
            continue;
        }
        let mut data = std::ptr::null_mut();
        let mut frames = 0;
        let mut flags = 0;
        let mut position = 0;
        let mut captured_qpc_100ns = 0;
        let vtable = source.input.vtable();
        let interface = source.input.as_raw();
        // SAFETY: outputs outlive the call; raw HRESULT avoids allocating COM errors while borrowing audio.
        let acquired = unsafe {
            (vtable.GetBuffer)(
                interface,
                &raw mut data,
                &raw mut frames,
                &raw mut flags,
                &raw mut position,
                &raw mut captured_qpc_100ns,
            )
        };
        if acquired.is_err() {
            return Err(MicError::Unavailable);
        }
        let bytes = usize::try_from(frames).ok().and_then(|n| {
            n.checked_mul(usize::from(source.format.channels) * source.format.bytes())
        });
        let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned() != 0;
        let status = PacketStatus {
            frames,
            position,
            captured_at: captured_qpc_100ns,
            discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0.cast_unsigned() != 0,
            timestamp_error: flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0.cast_unsigned() != 0,
        };
        let allowed = boundary.packet(token.capture_cutoff(session), status);
        let copied = match bytes {
            Some(n) if n <= raw.len() && allowed == Ok(true) && (silent || !data.is_null()) => {
                let borrowed = if silent {
                    None
                } else {
                    // SAFETY: WASAPI owns at least frames * block_align readable bytes until ReleaseBuffer.
                    Some(unsafe { std::slice::from_raw_parts(data, n) })
                };
                copy_batch(
                    &mut raw[..n],
                    borrowed,
                    if source.format.sample == Sample::U8 {
                        128
                    } else {
                        0
                    },
                    || token.collecting(session),
                    #[cfg(any(test, feature = "test-support"))]
                    || {},
                )
            }
            _ => 0,
        };
        // SAFETY: balances the one successful GetBuffer; no borrowed slice survives this call.
        let released = unsafe { (vtable.ReleaseBuffer)(interface, frames) };
        if released.is_err() {
            return Err(teardown_error(released));
        }
        let allowed = allowed?;
        if bytes.is_none_or(|n| n > raw.len())
            || allowed && frames != 0 && !silent && data.is_null()
        {
            return Err(MicError::Overflow);
        }
        if copied != 0 {
            boundary.kept(status, now);
        }
        // The gate and session are still read per frame; only the deadline clock is per copy.
        let copied_at = Instant::now();
        for frame in
            raw[..copied].chunks_exact(source.format.bytes() * usize::from(source.format.channels))
        {
            token.mode(session, copied_at)?;
            packets.push(source.format.mono(frame)?, output)?;
        }
        raw[..copied].zeroize();
    }
}

#[cfg(test)]
#[path = "microphone_format_tests.rs"]
mod format_tests;
