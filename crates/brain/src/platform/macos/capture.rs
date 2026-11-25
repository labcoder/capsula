//! The macOS screen capturer: `ScreenCaptureKit`'s one-shot screenshots, on macOS 14 and later.
//! The framework loads at runtime rather than at launch, so the brain still starts on older
//! systems, where capture refuses `Unsupported`.

use std::ffi::CStr;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect};
use objc2::runtime::{AnyClass, Sel};

use crate::screen::{Acquired, Capturer, Deadline, Failure, RawFrame, Refusal, byte_len};

mod chain;
mod layout;
mod native;

use chain::{Clock, Request, Sck};
use native::{Domain, Native};

/// How long a stopped or late chain gets to show it finished before the worker retires.
const GRACE: Duration = Duration::from_millis(200);

/// The display the last successful capture sampled and its bounds, so the seal accepts that
/// display alone and not another that moved into its place.
static CAPTURED: Mutex<Option<(u32, PhysicalRect)>> = Mutex::new(None);

/// The first macOS major version with `SCScreenshotManager`.
const FIRST_MAJOR: u32 = 14;

const FRAMEWORK: &CStr = c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit";

/// The classes and selectors captures use: `true` for a class method.
const REQUIRED: &[(&CStr, bool, &CStr)] = &[
    (
        c"SCScreenshotManager",
        true,
        c"captureImageWithFilter:configuration:completionHandler:",
    ),
    (
        c"SCShareableContent",
        true,
        c"getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:",
    ),
    (
        c"SCContentFilter",
        false,
        c"initWithDisplay:excludingApplications:exceptingWindows:",
    ),
    (
        c"SCContentFilter",
        false,
        c"initWithDesktopIndependentWindow:",
    ),
    (c"SCStreamConfiguration", false, c"setWidth:"),
    (c"SCStreamConfiguration", false, c"setHeight:"),
    (c"SCStreamConfiguration", false, c"setShowsCursor:"),
    (c"SCStreamConfiguration", false, c"setPixelFormat:"),
    (c"SCShareableContent", false, c"displays"),
    (c"SCShareableContent", false, c"applications"),
    (c"SCDisplay", false, c"displayID"),
    (c"SCDisplay", false, c"frame"),
    (c"SCRunningApplication", false, c"processID"),
];

/// `ScreenCaptureKit`'s capturer. A display capture runs; a window capture refuses with the
/// trait's defaults until its path lands.
struct ScreenCaptureKit<S> {
    sck: Arc<S>,
    clock: Clock,
}

impl<S: Sck> Capturer for ScreenCaptureKit<S> {
    /// The order the design fixed: the stamp's zero check, the layout and the size, the shell's
    /// identity, then the deadline and the grant, with the last deadline check inside the query's
    /// submission. The callback checks again and matches the display's frame, the result is
    /// checked against the deadline on receipt, and the same display must still be under the
    /// point, unmoved, before the image is drawn.
    fn capture_display(
        &mut self,
        point: PhysicalPoint,
        shell: u32,
        excluded: u64,
        deadline: Deadline,
        frame: &mut RawFrame,
    ) -> Result<(Acquired, PhysicalRect), Failure> {
        if let Ok(mut captured) = CAPTURED.lock() {
            *captured = None;
        }
        frame.fill(|frame| {
            if excluded == 0 {
                return Err(Refusal::Expired.into());
            }
            let display = layout::display_at(&self.sck.displays()?, point)?;
            let size = display.bounds.size;
            byte_len(size).ok_or(Refusal::Invalid)?;
            if shell == 0 {
                return Err(Refusal::Excluded.into());
            }
            let live =
                |deadline: Deadline| deadline.tighten(Instant::now()).ok_or(Refusal::Expired);
            let deadline = live(deadline)?;
            if !self.sck.preflight() {
                return Err(Refusal::NotPermitted.into());
            }
            let request = Request {
                display: display.id,
                origin: display.origin,
                points: display.points,
                size,
                shell,
            };
            let image = chain::run(&self.sck, deadline, request, self.clock)?;
            let arrived = Instant::now();
            if layout::display_at(&self.sck.displays()?, point)? != display {
                return Err(Refusal::DisplayChanged.into());
            }
            self.sck
                .draw(&image, size, frame.begin(size[0], size[1])?)?;
            if let Ok(mut captured) = CAPTURED.lock() {
                *captured = Some((display.id, display.bounds));
            }
            Ok((
                Acquired {
                    arrived,
                    dpi: display.dpi,
                },
                display.bounds,
            ))
        })
    }
}

/// Opens the capturer after capability checks only: no grant query and no capture.
///
/// # Errors
/// `Unsupported` before macOS 14, or when the framework or a class or selector it needs is
/// missing.
pub(crate) fn screen_capturer() -> Result<Box<dyn Capturer>, Failure> {
    admit(os_major, || loaded().is_some())?;
    let domain = loaded().ok_or(Refusal::Unsupported)?;
    Ok(Box::new(ScreenCaptureKit {
        sck: Arc::new(Native::new(domain)),
        clock: Clock {
            now: Instant::now,
            continuous: super::clock::counter,
            grace: GRACE,
        },
    }))
}

/// The bounds and DPI of the last captured display when it still has exactly `bounds` and the
/// arrangement still maps.
pub(crate) fn display_at(bounds: PhysicalRect) -> Option<(PhysicalRect, u32)> {
    let (id, captured) = (*CAPTURED.lock().ok()?)?;
    if captured != bounds {
        return None;
    }
    layout::covering(&native::displays().ok()?, id, bounds)
}

/// The OS version decides first, so an older system never loads the framework.
fn admit(
    os_major: impl FnOnce() -> Option<u32>,
    loaded: impl FnOnce() -> bool,
) -> Result<(), Failure> {
    if os_major().is_some_and(|major| major >= FIRST_MAJOR) && loaded() {
        Ok(())
    } else {
        Err(Refusal::Unsupported.into())
    }
}

fn os_major() -> Option<u32> {
    let mut version = [0_u8; 32];
    let mut len = version.len();
    // SAFETY: The name is NUL-terminated, `version` and `len` describe one writable buffer, and
    // no new value is set.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            version.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    major(version.get(..len)?)
}

/// The major number of a NUL-terminated product version such as `26.5.1`.
fn major(version: &[u8]) -> Option<u32> {
    let text = CStr::from_bytes_until_nul(version).ok()?.to_str().ok()?;
    text.split('.').next()?.parse().ok()
}

/// Loads the framework once for the process's life, checks every class and selector, and reads
/// its stream error domain.
fn loaded() -> Option<Domain> {
    static LOADED: OnceLock<Option<Domain>> = OnceLock::new();
    *LOADED.get_or_init(|| {
        // SAFETY: The path is NUL-terminated, and the handle is never closed, so nothing it loads
        // is unloaded while in use.
        let handle =
            unsafe { libc::dlopen(FRAMEWORK.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        let complete = !handle.is_null()
            && REQUIRED.iter().all(|&(class, class_method, selector)| {
                AnyClass::get(class).is_some_and(|class| {
                    let class = if class_method {
                        class.metaclass()
                    } else {
                        class
                    };
                    class.responds_to(Sel::register(selector))
                })
            });
        if !complete {
            return None;
        }
        // SAFETY: The handle is open and the name is NUL-terminated.
        let symbol = unsafe { libc::dlsym(handle, c"SCStreamErrorDomain".as_ptr()) };
        // SAFETY: The framework exports this name as its `NSString *const` error domain, and it
        // stays loaded.
        unsafe { Domain::read(NonNull::new(symbol.cast())?) }
    })
}

#[cfg(test)]
mod tests;
