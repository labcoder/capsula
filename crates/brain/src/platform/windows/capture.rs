//! Windows Graphics Capture of one display into the worker's reused buffer, ported from the
//! owner's S4 spike. One D3D11 device lives as long as the worker; each ticket gets its own
//! capture session, so nothing streams between tickets. macOS: `ScreenCaptureKit`'s
//! `SCScreenshotManager` behind the same `Capturer` seam.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{DisplayRef, PhysicalPoint};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_NOTIMPL, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_WAS_STILL_DRAWING, IDXGIDevice,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromRect,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::core::{Interface, factory};

use crate::screen::{
    Acquired, Capturer, Exclusion, Failure, RawFrame, Refusal, Target, display_edges,
};

const POLL: Duration = Duration::from_millis(1);

/// Opens the worker's capturer on its serve thread.
///
/// # Errors
/// The platform's HRESULT when capture is unsupported or the device can't be created.
pub(crate) fn screen_capturer() -> Result<Box<dyn Capturer>, i32> {
    Wgc::new()
        .map(|wgc| -> Box<dyn Capturer> { Box::new(wgc) })
        .map_err(|error| error.code().0)
}

struct Wgc {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt: IDirect3DDevice,
}

/// A lost device retires the worker, so the next ticket gets a fresh device chain.
fn fault(error: &windows::core::Error) -> Failure {
    let code = error.code();
    if [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DEVICE_HUNG,
    ]
    .contains(&code)
    {
        Failure::Retire(code.0)
    } else {
        Failure::Fault(code.0)
    }
}

impl Wgc {
    fn new() -> windows::core::Result<Self> {
        let awareness =
            // SAFETY: changes only this worker's DPI awareness, so rectangles are physical pixels.
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if let Err(error) = awareness {
            // Already set is fine; any other mismatch fails the exact geometry check later.
            tracing::debug!(code = error.code().0, "dpi awareness unchanged");
        }
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(E_NOTIMPL.into());
        }
        let (mut device, mut context) = (None, None);
        // SAFETY: a null adapter selects the hardware driver; both outputs are writable.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
        }?;
        let device: ID3D11Device = device.ok_or_else(|| windows::core::Error::from(E_NOTIMPL))?;
        let context = context.ok_or_else(|| windows::core::Error::from(E_NOTIMPL))?;
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: `dxgi` is the live device this owner keeps.
        let winrt = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;
        Ok(Self {
            device,
            context,
            winrt,
        })
    }

    /// One frame of `item` into `frame`: a session of its own, closed before the pixels are read.
    fn grab(
        &self,
        item: &GraphicsCaptureItem,
        size: SizeInt32,
        extent: [u32; 2],
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Instant, Failure> {
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &self.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            size,
        )
        .map_err(|e| fault(&e))?;
        let staged = pool
            .CreateCaptureSession(item)
            .map_err(|e| fault(&e))
            .and_then(|session| {
                let staged = self.acquire(&pool, &session, size, extent, until);
                session.Close().map_err(|e| Failure::Retire(e.code().0))?;
                staged
            });
        pool.Close().map_err(|e| Failure::Retire(e.code().0))?;
        let (arrived, staging) = staged?;
        self.read_out(&staging, extent, until, frame)?;
        Ok(arrived)
    }

    /// Starts the session and stages the first frame that's ready before `until`.
    fn acquire(
        &self,
        pool: &Direct3D11CaptureFramePool,
        session: &GraphicsCaptureSession,
        size: SizeInt32,
        extent: [u32; 2],
        until: Instant,
    ) -> Result<(Instant, ID3D11Texture2D), Failure> {
        // shortcut: without borderless access Windows keeps its yellow border; it's drawn on
        // screen, never in the frame, so the capture goes on.
        if let Err(error) = session.SetIsBorderRequired(false) {
            tracing::debug!(code = error.code().0, "capture border kept");
        }
        session
            .SetIsCursorCaptureEnabled(false)
            .map_err(|e| fault(&e))?;
        if Instant::now() >= until {
            return Err(Refusal::Expired.into());
        }
        session.StartCapture().map_err(|e| fault(&e))?;
        let captured = poll(
            until,
            || match pool.TryGetNextFrame() {
                Ok(captured) => Ok(Some(captured)),
                Err(error) if error.code().is_ok() => Ok(None),
                Err(error) => Err(fault(&error)),
            },
            |late| late.Close().map_err(|e| Failure::Retire(e.code().0)),
        )?;
        let arrived = Instant::now();
        let staged = self.stage(&captured, size, extent);
        captured.Close().map_err(|e| Failure::Retire(e.code().0))?;
        staged.map(|staging| (arrived, staging))
    }

    /// Issues the GPU copy of the frame's requested rectangle into a CPU-readable texture.
    fn stage(
        &self,
        captured: &Direct3D11CaptureFrame,
        size: SizeInt32,
        [width, height]: [u32; 2],
    ) -> Result<ID3D11Texture2D, Failure> {
        if captured.ContentSize().map_err(|e| fault(&e))? != size {
            return Err(Refusal::DisplayChanged.into());
        }
        let access: IDirect3DDxgiInterfaceAccess = captured
            .Surface()
            .and_then(|surface| surface.cast())
            .map_err(|e| fault(&e))?;
        // SAFETY: WGC exposes the frame's live texture through this documented interface.
        let source: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(|e| fault(&e))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `source` is live and `desc` is writable.
        unsafe { source.GetDesc(&raw mut desc) };
        if desc.Width < width || desc.Height < height || desc.SampleDesc.Count != 1 {
            return Err(Refusal::DisplayChanged.into());
        }
        desc.Width = width;
        desc.Height = height;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags =
            u32::try_from(D3D11_CPU_ACCESS_READ.0).map_err(|_| Failure::Fault(0))?;
        desc.MiscFlags = 0;
        let mut staging = None;
        // SAFETY: the descriptor, derived from the source, defines a CPU-readable texture.
        unsafe {
            self.device
                .CreateTexture2D(&raw const desc, None, Some(&raw mut staging))
        }
        .map_err(|e| fault(&e))?;
        let staging = staging.ok_or(Failure::Fault(E_NOTIMPL.0))?;
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
            front: 0,
            back: 1,
        };
        // SAFETY: the box lies inside both textures, which share a format.
        unsafe {
            self.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &source,
                0,
                Some(&raw const region),
            );
        }
        // SAFETY: submits the copy on this owner's context.
        unsafe { self.context.Flush() };
        Ok(staging)
    }

    /// Maps the staged copy and packs it into `frame`, once the capture has stopped.
    fn read_out(
        &self,
        staging: &ID3D11Texture2D,
        [width, height]: [u32; 2],
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<(), Failure> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let flags = u32::try_from(D3D11_MAP_FLAG_DO_NOT_WAIT.0).map_err(|_| Failure::Fault(0))?;
        let _mapping = poll(
            until,
            || {
                // SAFETY: subresource zero exists and isn't mapped; `mapped` is writable.
                match unsafe {
                    self.context
                        .Map(staging, 0, D3D11_MAP_READ, flags, Some(&raw mut mapped))
                } {
                    Ok(()) => Ok(Some(Mapping {
                        context: &self.context,
                        texture: staging,
                    })),
                    Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => Ok(None),
                    Err(error) => Err(fault(&error)),
                }
            },
            |_late| Ok(()),
        )?;
        copy_rows(&mapped, width, height, frame)
    }
}

/// Polls `attempt` until it's ready. A result that's ready at or after `until` goes to `late`
/// to be released and is refused.
fn poll<T>(
    until: Instant,
    mut attempt: impl FnMut() -> Result<Option<T>, Failure>,
    late: impl FnOnce(T) -> Result<(), Failure>,
) -> Result<T, Failure> {
    loop {
        let ready = attempt()?;
        if Instant::now() >= until {
            if let Some(value) = ready {
                late(value)?;
            }
            return Err(Refusal::Expired.into());
        }
        if let Some(value) = ready {
            return Ok(value);
        }
        std::thread::sleep(POLL);
    }
}

/// Copies the mapped staging texture row by row into exactly `width * height * 4` bytes.
fn copy_rows(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
    frame: &mut RawFrame,
) -> Result<(), Failure> {
    let pitch = usize::try_from(mapped.RowPitch).map_err(|_| Failure::Fault(0))?;
    let dest = frame.begin(width, height)?;
    let row = dest.len() / usize::try_from(height).map_err(|_| Failure::Fault(0))?;
    if mapped.pData.is_null() || pitch < row {
        return Err(Failure::Fault(E_NOTIMPL.0));
    }
    for (index, out) in dest.chunks_exact_mut(row).enumerate() {
        let offset = index.checked_mul(pitch).ok_or(Failure::Fault(0))?;
        // SAFETY: a mapped 2D staging texture spans `RowPitch` bytes per row for its `height`
        // rows, so this row's start lies inside the mapping.
        let start = unsafe { mapped.pData.cast::<u8>().add(offset) };
        // SAFETY: `row <= RowPitch` bytes from a row's start stay inside the mapping, which
        // nothing writes while it's held.
        let bytes = unsafe { std::slice::from_raw_parts(start, row) };
        out.copy_from_slice(bytes);
    }
    Ok(())
}

struct Mapping<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for Mapping<'_> {
    fn drop(&mut self) {
        // SAFETY: made only after a successful Map; it holds both resources.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

impl Capturer for Wgc {
    fn foreground(&mut self) -> Option<crate::screen::TopWindow> {
        super::elements::foreground()
    }
    fn capture_into(
        &mut self,
        display: &DisplayRef,
        exclude: Exclusion,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        // The shell's exclusion lease keeps Kumochi out of the frame (B3a).
        let Exclusion::KumochiWindows = exclude;
        let [left, top, right, bottom] = display_edges(display)?;
        let rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        // SAFETY: reads which monitor holds a valid rectangle.
        let monitor = unsafe { MonitorFromRect(&raw const rect, MONITOR_DEFAULTTONULL) };
        if monitor.is_invalid() {
            return Err(Refusal::DisplayChanged.into());
        }
        let mut info = MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFO>()).map_err(|_| Failure::Fault(0))?,
            ..MONITORINFO::default()
        };
        // SAFETY: `info` is writable and its size is set.
        let read = unsafe { GetMonitorInfoW(monitor, &raw mut info) };
        if !read.as_bool() || info.rcMonitor != rect {
            return Err(Refusal::DisplayChanged.into());
        }
        let (mut dpi, mut unused) = (0, 0);
        // SAFETY: both outputs are writable.
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut unused) }
            .map_err(|e| fault(&e))?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, _>().map_err(|e| fault(&e))?;
        // SAFETY: `monitor` came from MonitorFromRect just now.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(monitor) }.map_err(|e| fault(&e))?;
        let size = item.Size().map_err(|e| fault(&e))?;
        if [size.Width, size.Height] != [right - left, bottom - top] {
            return Err(Refusal::DisplayChanged.into());
        }
        let arrived = self.grab(&item, size, display.0.size, until, frame)?;
        Ok(Acquired { arrived, dpi })
    }

    fn pointer_window(&mut self, point: PhysicalPoint, shell: u32) -> Result<Target, Refusal> {
        super::window::pointer_window(point, shell)
    }

    /// B8's order: the walk's Kumochi check, then the item, then the same (pid, tid) again; a
    /// close or any change before the frame is read refuses.
    fn capture_window(
        &mut self,
        target: &Target,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        let window = super::window::handle(target)?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, _>().map_err(|e| fault(&e))?;
        // SAFETY: Windows validates the handle the walk resolved.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(window) }.map_err(|e| fault(&e))?;
        if !super::window::unchanged(target) {
            return Err(Refusal::WindowChanged.into());
        }
        let size = item.Size().map_err(|e| fault(&e))?;
        let extent = target.top.rect.size;
        if [size.Width, size.Height] != extent.map(|side| i32::try_from(side).unwrap_or(-1)) {
            return Err(Refusal::WindowChanged.into());
        }
        let closed = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&closed);
        let token = item
            .Closed(&TypedEventHandler::new(move |_, _| {
                seen.store(true, Ordering::Release);
                Ok(())
            }))
            .map_err(|e| fault(&e))?;
        let grabbed = self.grab(&item, size, extent, until, frame);
        item.RemoveClosed(token).map_err(|e| fault(&e))?;
        let arrived = grabbed?;
        if closed.load(Ordering::Acquire) || !super::window::unchanged(target) {
            return Err(Refusal::WindowChanged.into());
        }
        Ok(Acquired {
            arrived,
            dpi: target.dpi,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kumochi_protocol::geometry::{DisplayRef, PhysicalRect};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    };

    use super::{Wgc, copy_rows, fault, poll};
    use crate::screen::{CaptureId, Capturer, Exclusion, Failure, FrameMeta, RawFrame, Refusal};

    #[test]
    fn a_result_ready_at_the_deadline_is_released_and_refused() {
        let mut released = Vec::new();
        let late = poll(
            Instant::now(),
            || Ok(Some(7)),
            |value| {
                released.push(value);
                Ok(())
            },
        );
        assert_eq!(late, Err(Failure::Refused(Refusal::Expired)));
        assert_eq!(released, [7]);
        let failed_release = poll(Instant::now(), || Ok(Some(7)), |_| Err(Failure::Retire(-7)));
        assert_eq!(failed_release, Err(Failure::Retire(-7)));
    }

    #[test]
    fn poll_returns_the_first_ready_result_and_ends_at_the_deadline() {
        let far = Instant::now() + Duration::from_secs(60);
        let mut tries = 0;
        let ready = poll(
            far,
            || {
                tries += 1;
                Ok((tries == 3).then_some(tries))
            },
            |_| panic!("released a result ready before the deadline"),
        );
        assert_eq!(ready, Ok(3));
        let never = poll(
            Instant::now() + Duration::from_millis(5),
            || Ok(None::<u8>),
            |_| Ok(()),
        );
        assert_eq!(never, Err(Failure::Refused(Refusal::Expired)));
        let fault = poll(far, || Err::<Option<u8>, _>(Failure::Fault(-5)), |_| Ok(()));
        assert_eq!(fault, Err(Failure::Fault(-5)));
    }

    #[test]
    fn device_loss_retires_and_other_faults_do_not() {
        let removed = 0x887A_0005_u32.cast_signed();
        let hung = 0x887A_0006_u32.cast_signed();
        let reset = 0x887A_0007_u32.cast_signed();
        let still_drawing = 0x887A_000A_u32.cast_signed();
        let e_fail = 0x8000_4005_u32.cast_signed();
        for (code, expected) in [
            (removed, Failure::Retire(removed)),
            (hung, Failure::Retire(hung)),
            (reset, Failure::Retire(reset)),
            (still_drawing, Failure::Fault(still_drawing)),
            (e_fail, Failure::Fault(e_fail)),
        ] {
            let error = windows::core::Error::from(windows::core::HRESULT(code));
            assert_eq!(fault(&error), expected, "{code:#x}");
        }
    }

    #[test]
    fn copy_rows_packs_rows_without_padding_and_rejects_bad_mappings() {
        // Three rows of two pixels, each padded from 8 to 12 bytes.
        let mut texture: [u8; 36] = [
            1, 2, 3, 4, 5, 6, 7, 8, 0xEE, 0xEE, 0xEE, 0xEE, //
            11, 12, 13, 14, 15, 16, 17, 18, 0xEE, 0xEE, 0xEE, 0xEE, //
            21, 22, 23, 24, 25, 26, 27, 28, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        let packed: [u8; 24] = [
            1, 2, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18, 21, 22, 23, 24, 25, 26, 27, 28,
        ];
        let data = texture.as_mut_ptr().cast();
        let mapped = |data, pitch| D3D11_MAPPED_SUBRESOURCE {
            pData: data,
            RowPitch: pitch,
            DepthPitch: pitch * 3,
        };
        let meta = FrameMeta {
            capture: CaptureId(1),
            display: PhysicalRect {
                origin: [0, 0],
                size: [2, 3],
            },
            dpi: 96,
            acquired_after_ms: 0,
        };
        let not_implemented = Err(Failure::Fault(0x8000_4001_u32.cast_signed()));
        let mut frame = RawFrame::default();
        for bad in [mapped(std::ptr::null_mut(), 12), mapped(data, 7)] {
            assert_eq!(copy_rows(&mapped(data, 12), 2, 3, &mut frame), Ok(()));
            frame.commit(meta).expect("a full copy commits");
            assert_eq!(frame.image(), Some((meta, &packed[..])));
            assert_eq!(copy_rows(&bad, 2, 3, &mut frame), not_implemented);
            assert_eq!(frame.image(), None);
        }
    }

    fn primary() -> DisplayRef {
        // SAFETY: asks which monitor is primary; no pointers.
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFO>()).expect("small struct"),
            ..MONITORINFO::default()
        };
        // SAFETY: `info` is writable and its size is set.
        let read = unsafe { GetMonitorInfoW(monitor, &raw mut info) };
        assert!(read.as_bool(), "monitor info");
        let r = info.rcMonitor;
        DisplayRef(PhysicalRect {
            origin: [r.left, r.top],
            size: [
                u32::try_from(r.right - r.left).expect("width"),
                u32::try_from(r.bottom - r.top).expect("height"),
            ],
        })
    }

    fn sample(wgc: &mut Wgc, display: &DisplayRef, frame: &mut RawFrame) -> (Duration, u32) {
        let start = Instant::now();
        let until = start + Duration::from_secs(2);
        let acquired = wgc
            .capture_into(display, Exclusion::KumochiWindows, until, frame)
            .expect("capture");
        (start.elapsed(), acquired.dpi)
    }

    /// Rez evidence, run by hand on a quiet machine: `--ignored --nocapture`. Pixels stay in memory.
    #[test]
    #[ignore = "Rez only: captures the primary display, in memory"]
    fn primary_display_cold_and_warm_samples() {
        let display = primary();
        let mut frame = RawFrame::default();
        let mut cold = Vec::new();
        for _ in 0..20 {
            let start = Instant::now();
            let mut wgc = Wgc::new().expect("device");
            let (_, dpi) = sample(&mut wgc, &display, &mut frame);
            cold.push(start.elapsed());
            println!("display {:?} dpi {dpi}", display.0);
        }
        let mut wgc = Wgc::new().expect("device");
        sample(&mut wgc, &display, &mut frame);
        let mut warm: Vec<_> = (0..20)
            .map(|_| sample(&mut wgc, &display, &mut frame).0)
            .collect();
        for (name, times) in [("cold", &mut cold), ("warm", &mut warm)] {
            times.sort();
            println!(
                "{name}: p50 {:?} p95 {:?} max {:?} n {}",
                times[9],
                times[18],
                times[19],
                times.len()
            );
        }
    }
}
