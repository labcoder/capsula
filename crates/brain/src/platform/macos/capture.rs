//! The macOS screen capturer: `ScreenCaptureKit`'s one-shot screenshots, on macOS 14 and later.
//! The framework loads at runtime rather than at launch, so the brain still starts on older
//! systems, where capture refuses `Unsupported`.

use std::ffi::CStr;
use std::sync::OnceLock;

use objc2::runtime::{AnyClass, Sel};

use crate::screen::{Capturer, Failure, Refusal};

/// The first macOS major version with `SCScreenshotManager`.
const FIRST_MAJOR: u32 = 14;

const FRAMEWORK: &CStr = c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit";

/// The classes and selectors captures use: `true` for a class method.
const REQUIRED: [(&CStr, bool, &CStr); 7] = [
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
];

/// `ScreenCaptureKit`'s capturer. Its captures land with the display and window paths; until
/// then the trait's refusals answer.
struct ScreenCaptureKit;

impl Capturer for ScreenCaptureKit {}

/// Opens the capturer after capability checks only: no grant query and no capture.
///
/// # Errors
/// `Unsupported` before macOS 14, or when the framework or a class or selector it needs is
/// missing.
pub(crate) fn screen_capturer() -> Result<Box<dyn Capturer>, Failure> {
    admit(os_major, loaded)?;
    Ok(Box::new(ScreenCaptureKit))
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

/// Loads the framework once for the process's life, then checks every class and selector.
fn loaded() -> bool {
    static LOADED: OnceLock<bool> = OnceLock::new();
    *LOADED.get_or_init(|| {
        // SAFETY: The path is NUL-terminated, and the handle is never closed, so nothing it loads
        // is unloaded while in use.
        let handle =
            unsafe { libc::dlopen(FRAMEWORK.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        !handle.is_null()
            && REQUIRED.iter().all(|&(class, class_method, selector)| {
                AnyClass::get(class).is_some_and(|class| {
                    let class = if class_method {
                        class.metaclass()
                    } else {
                        class
                    };
                    class.responds_to(Sel::register(selector))
                })
            })
    })
}

#[cfg(test)]
mod tests;
