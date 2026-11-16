//! Windows Graphics Capture of one window or one display into the worker's reused buffer, ported
//! from the owner's S4 spike. One D3D11 device lives as long as the worker; each ticket gets its own
//! capture session, so nothing streams between tickets. macOS: `ScreenCaptureKit`'s
//! `SCScreenshotManager` behind the same `Capturer` seam.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_NOTIMPL, HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_WAS_STILL_DRAWING, IDXGIDevice,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO,
    MonitorFromPoint, MonitorFromRect,
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

use super::clock::{qpc_hns, qpc_hns_up, qpc_now, qpc_ticks};
use crate::screen::{Acquired, Capturer, Failure, RawFrame, Refusal, Target};

const POLL: Duration = Duration::from_millis(1);
/// The redraw a display frame waits out when the compositor's timing can't be read.
const UNREAD_REDRAW: Duration = Duration::from_millis(100);

/// What one capture asks of its session: whether the pointer is drawn, and the earliest draw
/// time a frame may have, in the 100-ns units Windows stamps frames with.
#[derive(Clone, Copy)]
struct Shot {
    cursor: bool,
    not_before: Option<u64>,
}

impl Shot {
    const WINDOW: Self = Self {
        cursor: false,
        not_before: None,
    };
}

/// Whether this platform captures one window alone: Windows Graphics Capture does.
pub(crate) const WINDOW_CAPTURE: bool = true;

/// Opens the worker's capturer on its serve thread.
///
/// # Errors
/// `Unsupported` when the system says it can't capture a window, else the platform's HRESULT
/// as a fault.
pub(crate) fn screen_capturer() -> Result<Box<dyn Capturer>, Failure> {
    supported(GraphicsCaptureSession::IsSupported())?;
    Wgc::new()
        .map(|wgc| -> Box<dyn Capturer> { Box::new(wgc) })
        .map_err(|error| Failure::Fault(error.code().0))
}

/// Only the system's no reads as `Unsupported`; an error asking is a fault like any other, so no
/// HRESULT is ever read as the answer.
fn supported(answer: windows::core::Result<bool>) -> Result<(), Failure> {
    match answer {
        Ok(true) => Ok(()),
        Ok(false) => Err(Refusal::Unsupported.into()),
        Err(error) => Err(Failure::Fault(error.code().0)),
    }
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
        shot: Shot,
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
                let staged = self.acquire(&pool, &session, size, extent, shot, until);
                session.Close().map_err(|e| Failure::Retire(e.code().0))?;
                staged
            });
        pool.Close().map_err(|e| Failure::Retire(e.code().0))?;
        let (arrived, staging) = staged?;
        self.read_out(&staging, extent, until, frame)?;
        Ok(arrived)
    }

    /// Starts the session once the counter passes the shot's `not_before`, and stages the first
    /// frame drawn no earlier that's ready before `until`.
    fn acquire(
        &self,
        pool: &Direct3D11CaptureFramePool,
        session: &GraphicsCaptureSession,
        size: SizeInt32,
        extent: [u32; 2],
        shot: Shot,
        until: Instant,
    ) -> Result<(Instant, ID3D11Texture2D), Failure> {
        // shortcut: without borderless access Windows keeps its yellow border; it's drawn on
        // screen, never in the frame, so the capture goes on.
        if let Err(error) = session.SetIsBorderRequired(false) {
            tracing::debug!(code = error.code().0, "capture border kept");
        }
        session
            .SetIsCursorCaptureEnabled(shot.cursor)
            .map_err(|e| fault(&e))?;
        start_after(shot.not_before, until, now_hns, || {
            session.StartCapture().map_err(|e| fault(&e))
        })?;
        let captured = poll(
            until,
            || match pool.TryGetNextFrame() {
                Ok(captured) => drawn_after(shot.not_before, captured, drawn, |early| {
                    early.Close().map_err(|e| Failure::Retire(e.code().0))
                }),
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

/// The bounds and effective DPI of the monitor that exactly covers `bounds` now, if any.
pub(crate) fn display_at(bounds: PhysicalRect) -> Option<(PhysicalRect, u32)> {
    let [left, top] = bounds.origin;
    let [width, height] = bounds.size.map(|side| i32::try_from(side).ok());
    let rect = RECT {
        left,
        top,
        right: left.checked_add(width?)?,
        bottom: top.checked_add(height?)?,
    };
    // SAFETY: `rect` is a readable rectangle; asks which monitor holds it.
    let monitor = unsafe { MonitorFromRect(&raw const rect, MONITOR_DEFAULTTONULL) };
    geometry(monitor)
}

/// A monitor's physical bounds and effective DPI, or none for no monitor.
fn geometry(monitor: HMONITOR) -> Option<(PhysicalRect, u32)> {
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).ok()?,
        ..MONITORINFO::default()
    };
    // SAFETY: `info` is writable and its size is set.
    if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        return None;
    }
    let edges = info.rcMonitor;
    let bounds = PhysicalRect {
        origin: [edges.left, edges.top],
        size: [
            u32::try_from(edges.right - edges.left).ok()?,
            u32::try_from(edges.bottom - edges.top).ok()?,
        ],
    };
    let (mut dpi, mut unused) = (0, 0);
    // SAFETY: both outputs are writable.
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut unused) }.ok()?;
    Some((bounds, dpi))
}

/// Calls `start` once `now` reaches `not_before`, and never at or after `until`.
fn start_after(
    not_before: Option<u64>,
    until: Instant,
    mut now: impl FnMut() -> Result<u64, Failure>,
    start: impl FnOnce() -> Result<(), Failure>,
) -> Result<(), Failure> {
    if let Some(not_before) = not_before {
        poll(
            until,
            || Ok((now()? >= not_before).then_some(())),
            |()| Ok(()),
        )?;
    }
    if Instant::now() >= until {
        return Err(Refusal::Expired.into());
    }
    start()
}

/// Keeps a frame drawn at or after `not_before`. An earlier one is released, so the pool can
/// deliver the next.
fn drawn_after<T>(
    not_before: Option<u64>,
    frame: T,
    drawn: impl FnOnce(&T) -> Result<u64, Failure>,
    release: impl FnOnce(T) -> Result<(), Failure>,
) -> Result<Option<T>, Failure> {
    let Some(not_before) = not_before else {
        return Ok(Some(frame));
    };
    let time = match drawn(&frame) {
        Ok(time) => time,
        Err(failure) => {
            release(frame)?;
            return Err(failure);
        }
    };
    if time >= not_before {
        return Ok(Some(frame));
    }
    release(frame)?;
    Ok(None)
}

/// When Windows drew `captured`, in the 100-ns units of the performance counter's clock.
fn drawn(captured: &Direct3D11CaptureFrame) -> Result<u64, Failure> {
    let time = captured.SystemRelativeTime().map_err(|e| fault(&e))?;
    u64::try_from(time.Duration).map_err(|_| Failure::Fault(0))
}

/// The counter now, in a captured frame's units.
fn now_hns() -> Result<u64, Failure> {
    qpc_now().and_then(qpc_hns).ok_or(Failure::Fault(0))
}

/// The earliest draw time a display frame may have: two redraws after `excluded`, the counter
/// stamp taken once Kumochi's windows were kept out of capture, rounded up. A frame's draw time
/// is when it's presented, so one drawn a single redraw later may come from a composition that
/// began at about `excluded`. A zero stamp is no stamp.
fn not_before(excluded: u64, redraw: u64) -> Result<u64, Failure> {
    if excluded == 0 {
        return Err(Refusal::Expired.into());
    }
    let ticks = redraw
        .checked_mul(2)
        .and_then(|wait| excluded.checked_add(wait))
        .ok_or(Refusal::Expired)?;
    qpc_hns_up(ticks).ok_or(Failure::Fault(0))
}

/// One compositor redraw in counter ticks, or [`UNREAD_REDRAW`] when its timing can't be read.
fn redraw() -> Result<u64, Failure> {
    let mut timing = DWM_TIMING_INFO {
        cbSize: u32::try_from(size_of::<DWM_TIMING_INFO>()).map_err(|_| Failure::Fault(0))?,
        ..DWM_TIMING_INFO::default()
    };
    // SAFETY: a null window asks for the compositor's own timing; `timing` is writable and sized.
    match unsafe { DwmGetCompositionTimingInfo(HWND::default(), &raw mut timing) } {
        Ok(()) if timing.qpcRefreshPeriod > 0 => Ok(timing.qpcRefreshPeriod),
        _ => qpc_ticks(UNREAD_REDRAW).ok_or(Failure::Fault(0)),
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
    fn pointer_window(&mut self, point: PhysicalPoint, shell: u32) -> Result<Target, Refusal> {
        super::window::pointer_window(point, shell)
    }

    /// B8's order: the walk's Kumochi check, then the item, then the same identity again; a
    /// close or any change before the frame is read refuses. Every failure leaves `frame` empty.
    fn capture_window(
        &mut self,
        target: &Target,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        frame.fill(|frame| self.window_frame(target, until, frame))
    }

    fn capture_display(
        &mut self,
        point: PhysicalPoint,
        excluded: u64,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<(Acquired, PhysicalRect), Failure> {
        frame.fill(|frame| self.display_frame(point, excluded, until, frame))
    }
}

impl Wgc {
    fn window_frame(
        &self,
        target: &Target,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<Acquired, Failure> {
        let window = super::window::handle(target)?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, _>().map_err(|e| fault(&e))?;
        // SAFETY: Windows validates the handle the walk resolved.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(window) }.map_err(|e| {
                if super::window::unchanged(target) {
                    fault(&e)
                } else {
                    Refusal::WindowChanged.into()
                }
            })?;
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
        let grabbed = self.grab(&item, size, extent, Shot::WINDOW, until, frame);
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

    fn display_frame(
        &self,
        point: PhysicalPoint,
        excluded: u64,
        until: Instant,
        frame: &mut RawFrame,
    ) -> Result<(Acquired, PhysicalRect), Failure> {
        let not_before = not_before(excluded, redraw()?)?;
        let [x, y] = point.0;
        // SAFETY: asks which monitor is nearest a point; no pointers.
        let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
        let (bounds, dpi) = geometry(monitor).ok_or(Refusal::DisplayChanged)?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, _>().map_err(|e| fault(&e))?;
        // SAFETY: `monitor` came from MonitorFromPoint just now.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(monitor) }.map_err(|e| fault(&e))?;
        let size = item.Size().map_err(|e| fault(&e))?;
        if [size.Width, size.Height] != bounds.size.map(|side| i32::try_from(side).unwrap_or(-1)) {
            return Err(Refusal::DisplayChanged.into());
        }
        let shot = Shot {
            cursor: true,
            not_before: Some(not_before),
        };
        let arrived = self.grab(&item, size, bounds.size, shot, until, frame)?;
        Ok((Acquired { arrived, dpi }, bounds))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kumochi_protocol::geometry::PhysicalRect;
    use windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE;

    use kumochi_protocol::geometry::PhysicalPoint;
    use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::core::factory;

    use super::{
        Wgc, copy_rows, drawn, drawn_after, fault, not_before, now_hns, poll, start_after,
        supported,
    };
    use crate::platform::windows::clock::{qpc_hns_up, qpc_now};
    use crate::screen::{CaptureId, Capturer, Failure, FrameMeta, RawFrame, Refusal};

    #[test]
    fn only_the_systems_no_reads_as_unsupported() {
        let not_implemented = windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL);
        let failed = windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
        assert_eq!(supported(Ok(true)), Ok(()));
        assert_eq!(
            supported(Ok(false)),
            Err(Failure::Refused(Refusal::Unsupported))
        );
        assert_eq!(
            supported(Err(not_implemented)),
            Err(Failure::Fault(0x8000_4001_u32.cast_signed()))
        );
        assert_eq!(
            supported(Err(failed)),
            Err(Failure::Fault(0x8000_4005_u32.cast_signed()))
        );
    }

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
    fn a_frame_drawn_before_not_before_is_released_and_one_at_it_is_kept() {
        let mut released = Vec::new();
        let mut release = |frame| {
            released.push(frame);
            Ok(())
        };
        assert_eq!(
            drawn_after(Some(100), 'a', |_| Ok(99), &mut release),
            Ok(None)
        );
        assert_eq!(
            drawn_after(Some(100), 'b', |_| Ok(100), &mut release),
            Ok(Some('b'))
        );
        assert_eq!(
            drawn_after(None, 'c', |_| Ok(0), &mut release),
            Ok(Some('c'))
        );
        assert_eq!(
            drawn_after(Some(100), 'd', |_| Err(Failure::Fault(-3)), &mut release),
            Err(Failure::Fault(-3))
        );
        assert_eq!(
            released,
            ['a', 'd'],
            "a frame whose draw time can't be read is released"
        );
        assert_eq!(
            drawn_after(Some(100), 'e', |_| Ok(1), |_| Err(Failure::Retire(-4))),
            Err(Failure::Retire(-4))
        );
    }

    #[test]
    fn the_first_frame_drawn_late_enough_is_taken_and_early_ones_end_at_the_deadline() {
        let far = Instant::now() + Duration::from_secs(60);
        let mut queue = vec![(1, 99), (2, 100), (3, 101)].into_iter();
        let mut released = Vec::new();
        let taken = poll(
            far,
            || {
                let Some(frame) = queue.next() else {
                    return Ok(None);
                };
                drawn_after(
                    Some(100),
                    frame,
                    |frame| Ok(frame.1),
                    |frame| {
                        released.push(frame.0);
                        Ok(())
                    },
                )
            },
            |_| panic!("a frame ready before the deadline was refused"),
        );
        assert_eq!(taken, Ok((2, 100)));
        assert_eq!(released, [1]);
        let mut early = 0;
        let never = poll(
            Instant::now() + Duration::from_millis(5),
            || {
                drawn_after(
                    Some(100),
                    7,
                    |_| Ok(99),
                    |_| {
                        early += 1;
                        Ok(())
                    },
                )
            },
            |_| Ok(()),
        );
        assert_eq!(never, Err(Failure::Refused(Refusal::Expired)));
        assert!(
            early >= 1,
            "every early frame was released, {early} of them"
        );
    }

    #[test]
    fn the_session_starts_only_once_the_counter_reaches_not_before() {
        let far = Instant::now() + Duration::from_secs(60);
        let mut clock = [98, 99, 100, 101].into_iter();
        let read = std::cell::Cell::new(0);
        let mut started_at = None;
        let started = start_after(
            Some(100),
            far,
            || {
                read.set(read.get() + 1);
                Ok(clock.next().expect("the wait stops at 100"))
            },
            || {
                started_at = Some(read.get());
                Ok(())
            },
        );
        assert_eq!(started, Ok(()));
        assert_eq!(started_at, Some(3), "started after reading 100, not before");
        let mut started = false;
        assert_eq!(
            start_after(
                Some(100),
                Instant::now() + Duration::from_millis(5),
                || Ok(99),
                || {
                    started = true;
                    Ok(())
                }
            ),
            Err(Failure::Refused(Refusal::Expired))
        );
        assert!(!started, "a session never starts at the deadline");
        assert_eq!(
            start_after(
                None,
                Instant::now(),
                || Ok(0),
                || panic!("started at the deadline")
            ),
            Err(Failure::Refused(Refusal::Expired))
        );
        assert_eq!(start_after(None, far, || Ok(0), || Ok(())), Ok(()));
    }

    #[test]
    fn a_missing_or_overflowing_exclusion_stamp_refuses() {
        assert_eq!(not_before(0, 10), Err(Failure::Refused(Refusal::Expired)));
        assert_eq!(
            not_before(u64::MAX, 1),
            Err(Failure::Refused(Refusal::Expired))
        );
        assert_eq!(
            not_before(u64::MAX - 20, u64::MAX / 2 + 1),
            Err(Failure::Refused(Refusal::Expired))
        );
        assert_eq!(
            not_before(1_000, 10),
            Ok(qpc_hns_up(1_020).expect("counter"))
        );
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

    fn primary_monitor() -> (HMONITOR, PhysicalPoint) {
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
        (
            monitor,
            PhysicalPoint([r.left.midpoint(r.right), r.top.midpoint(r.bottom)]),
        )
    }

    /// Rez evidence, run by hand: `--ignored --nocapture`. Pixels stay in memory.
    #[test]
    #[ignore = "Rez only: captures the primary display, in memory"]
    fn primary_display_cold_and_warm_samples() {
        let (_, point) = primary_monitor();
        let mut frame = RawFrame::default();
        let sample = |wgc: &mut Wgc, frame: &mut RawFrame| {
            let start = Instant::now();
            let excluded = qpc_now().expect("counter");
            let (acquired, bounds) = wgc
                .capture_display(point, excluded, start + Duration::from_secs(2), frame)
                .expect("capture");
            (start.elapsed(), acquired.dpi, bounds.size)
        };
        let mut cold = Vec::new();
        for _ in 0..20 {
            let start = Instant::now();
            let mut wgc = Wgc::new().expect("device");
            let (_, dpi, size) = sample(&mut wgc, &mut frame);
            cold.push(start.elapsed());
            println!("display {size:?} dpi {dpi}");
        }
        let mut wgc = Wgc::new().expect("device");
        sample(&mut wgc, &mut frame);
        let mut warm: Vec<_> = (0..20).map(|_| sample(&mut wgc, &mut frame).0).collect();
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

    /// The first frame of a fresh display session: its draw time minus the session's start, in
    /// microseconds (negative is earlier), and how long it took to arrive.
    fn first_frame(wgc: &Wgc, monitor: HMONITOR) -> (i64, Duration) {
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, _>().expect("interop");
        // SAFETY: `monitor` came from MonitorFromPoint.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.expect("item");
        let size = item.Size().expect("size");
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &wgc.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            size,
        )
        .expect("pool");
        let session = pool.CreateCaptureSession(&item).expect("session");
        if let Err(error) = session.SetIsBorderRequired(false) {
            println!("border kept: {:#x}", error.code().0);
        }
        session.SetIsCursorCaptureEnabled(true).expect("cursor");
        let at = Instant::now();
        let started = now_hns().expect("counter");
        session.StartCapture().expect("start");
        let captured = poll(
            at + Duration::from_secs(2),
            || match pool.TryGetNextFrame() {
                Ok(captured) => Ok(Some(captured)),
                Err(error) if error.code().is_ok() => Ok(None),
                Err(error) => Err(fault(&error)),
            },
            |late| late.Close().map_err(|e| Failure::Retire(e.code().0)),
        )
        .expect("a first frame within 2 s");
        let arrived = at.elapsed();
        let drawn = drawn(&captured).expect("draw time");
        captured.Close().expect("frame closes");
        session.Close().expect("session closes");
        pool.Close().expect("pool closes");
        let offset = i64::try_from(drawn).expect("hns") - i64::try_from(started).expect("hns");
        (offset / 10, arrived)
    }

    /// Rez evidence for the frame-time check, run by hand on a still screen:
    /// `--ignored --nocapture`. Pixels stay in memory and are never read.
    #[test]
    #[ignore = "Rez only: captures the primary display, in memory"]
    fn first_display_frame_against_its_session_start() {
        let (monitor, _) = primary_monitor();
        let warm = Wgc::new().expect("device");
        first_frame(&warm, monitor);
        for cold in [true, false] {
            let samples: Vec<_> = (0..10)
                .map(|_| {
                    if cold {
                        first_frame(&Wgc::new().expect("device"), monitor)
                    } else {
                        first_frame(&warm, monitor)
                    }
                })
                .collect();
            let name = if cold { "cold" } else { "warm" };
            for (offset, arrived) in &samples {
                println!("{name}: drawn - started {offset} us, arrived after {arrived:?}");
            }
            let earlier = samples.iter().filter(|(offset, _)| *offset < 0).count();
            println!(
                "{name}: {earlier} of {} drawn before the session started",
                samples.len()
            );
        }
    }
}
