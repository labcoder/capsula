//! Input-only WASAPI polling worker. No callbacks, loopback, resampling or reopen.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use async_channel::Sender;
use kumochi_protocol::message::{MAX_MIC_SAMPLES, MicPcm};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eCommunications,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::core::{GUID, Interface};
use zeroize::{Zeroize, Zeroizing};

use crate::platform::microphone::{
    Finalization, Format, Generation, MicError, Mode, POLL, Packetizer, Running, Sample,
};

struct Apartment;
impl Apartment {
    fn new() -> Result<Self, MicError> {
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

struct Mix(*mut WAVEFORMATEX);
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
        // SAFETY: the mix format is live and validated. Flags exclude loopback and rate conversion.
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix.0, None) }
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

    fn stop(&mut self) -> Result<(), MicError> {
        if self.started {
            self.started = false;
            // SAFETY: only this worker starts/stops this client, and no native buffer is borrowed.
            unsafe { self.client.Stop() }.map_err(|_| MicError::Unconfirmed)?;
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

fn read_format(mix: &Mix) -> Result<Format, MicError> {
    if mix.0.is_null() {
        return Err(MicError::Format);
    }
    // SAFETY: GetMixFormat returns at least WAVEFORMATEX; read_unaligned handles packed layout.
    let wave = unsafe { mix.0.read_unaligned() };
    let (tag, valid) = if wave.wFormatTag == 0xfffe {
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
        (tag, valid)
    } else {
        (wave.wFormatTag, wave.wBitsPerSample)
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
    }
    .check()?;
    let align = format.bytes() * usize::from(format.channels);
    if usize::from(wave.nBlockAlign) != align
        || u64::from(wave.nAvgBytesPerSec)
            != u64::from(format.rate) * u64::try_from(align).map_err(|_| MicError::Format)?
    {
        return Err(MicError::Format);
    }
    Ok(format)
}

fn live(token: &Generation, session: &AtomicBool) -> Result<(), MicError> {
    match token.mode(session, Instant::now())? {
        Mode::Release => Err(MicError::Cancelled),
        Mode::Preparing | Mode::Collecting => Ok(()),
    }
}

pub(crate) fn capture(
    token: &Generation,
    session: &AtomicBool,
    running: &Sender<Running>,
    output: &Sender<MicPcm>,
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
    let mut result = collect(&mut source, token, session, running, output, &mut packets);
    if matches!(result, Err(MicError::Stale | MicError::Cancelled))
        && token.mode(session, Instant::now()) == Ok(Mode::Release)
    {
        result = Ok(());
    }
    if result.is_err() {
        token.cancel();
        packets.clear();
    }
    source.stop()?;
    drop(source);
    if result == Err(MicError::Unconfirmed) {
        return Err(MicError::Unconfirmed);
    }
    match result {
        Ok(()) => match token.mode(session, Instant::now()) {
            Ok(Mode::Release) => Ok(match packets.finish(output) {
                Ok(packets) => Finalization::Released { packets },
                Err(error) => {
                    token.cancel();
                    Finalization::Discarded(error)
                }
            }),
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
) -> Result<(), MicError> {
    live(token, session)?;
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
    let mut raw = Zeroizing::new(vec![
        0_u8;
        MAX_MIC_SAMPLES
            * usize::from(source.format.channels)
            * source.format.bytes()
    ]);
    let result = read(source, token, session, output, packets, &mut raw);
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
) -> Result<(), MicError> {
    let mut ready = false;
    let mut first = true;
    loop {
        let mode = token.mode(session, Instant::now())?;
        if mode == Mode::Release {
            return Ok(());
        }
        // SAFETY: pinned endpoint remains owned on this COM thread.
        if unsafe { source.device.GetState() }.map_err(|_| MicError::Unavailable)?
            != DEVICE_STATE_ACTIVE
        {
            return Err(MicError::Unavailable);
        }
        // SAFETY: the pinned client is live; Mix owns this off-buffer COM allocation.
        let mix = Mix(unsafe { source.client.GetMixFormat() }.map_err(|_| MicError::Format)?);
        if read_format(&mix)? != source.format {
            return Err(MicError::Format);
        }
        // SAFETY: no buffer is currently held; this is a nonblocking capture-size query.
        if unsafe { source.input.GetNextPacketSize() }.map_err(|_| MicError::Unavailable)? == 0 {
            ready = mode == Mode::Collecting;
            std::thread::park_timeout(POLL);
            continue;
        }
        let mut data = std::ptr::null_mut();
        let mut frames = 0;
        let mut flags = 0;
        let vtable = source.input.vtable();
        let interface = source.input.as_raw();
        // SAFETY: outputs outlive the call; raw HRESULT avoids allocating COM errors while borrowing audio.
        let acquired = unsafe {
            (vtable.GetBuffer)(
                interface,
                &raw mut data,
                &raw mut frames,
                &raw mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if acquired.is_err() {
            return Err(MicError::Unavailable);
        }
        let bytes = usize::try_from(frames).ok().and_then(|n| {
            n.checked_mul(usize::from(source.format.channels) * source.format.bytes())
        });
        let bad = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0.cast_unsigned() != 0
            || !first && flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0.cast_unsigned() != 0;
        let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned() != 0;
        let allowed = token.collecting(session) && ready;
        let copied = match bytes {
            Some(n) if n <= raw.len() && !bad && allowed && (silent || !data.is_null()) => {
                if silent {
                    raw[..n].fill(if source.format.sample == Sample::U8 {
                        128
                    } else {
                        0
                    });
                } else {
                    // SAFETY: WASAPI owns at least frames * block_align readable bytes until ReleaseBuffer.
                    let borrowed = unsafe { std::slice::from_raw_parts(data, n) };
                    raw[..n].copy_from_slice(borrowed);
                }
                n
            }
            _ => 0,
        };
        // SAFETY: balances the one successful GetBuffer; no borrowed slice survives this call.
        let released = unsafe { (vtable.ReleaseBuffer)(interface, frames) };
        if released.is_err() {
            return Err(MicError::Unconfirmed);
        }
        first = false;
        if bad {
            return Err(MicError::Discontinuity);
        }
        if bytes.is_none_or(|n| n > raw.len())
            || allowed && frames != 0 && !silent && data.is_null()
        {
            return Err(MicError::Overflow);
        }
        for frame in
            raw[..copied].chunks_exact(source.format.bytes() * usize::from(source.format.channels))
        {
            token.mode(session, Instant::now())?;
            packets.push(source.format.mono(frame)?, output)?;
        }
        raw[..copied].zeroize();
    }
}
