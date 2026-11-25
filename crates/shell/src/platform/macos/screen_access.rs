//! macOS Screen Recording access through CoreGraphics' documented calls (macOS 10.15+).
//! Access alone captures nothing; the brain's worker gates capture separately.

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

/// Whether macOS already lets this app record the screen. Never prompts.
pub(crate) fn granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Asks macOS for access. macOS may show its dialog, may not, and may answer false before the
/// person decides, so false is not a denial.
pub(crate) fn request() -> bool {
    CGRequestScreenCaptureAccess()
}
