//! How a worker fails: without a dialog, and on purpose in dev fixtures.

#[cfg(debug_assertions)]
use std::io;
#[cfg(debug_assertions)]
use std::path::Path;

#[cfg(debug_assertions)]
use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
#[cfg(debug_assertions)]
use windows_sys::Win32::System::Diagnostics::Debug::RaiseException;
use windows_sys::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
};
#[cfg(debug_assertions)]
use windows_sys::Win32::System::SystemServices::EXCEPTION_NONCONTINUABLE;

#[cfg(debug_assertions)]
use super::library::{Library, RawExport};

/// No error dialogs for this process, so a worker that crashes ends at once instead of waiting
/// on a dialog nobody sees.
pub fn quiet_failures() {
    // SAFETY: sets this process's error mode; it takes and returns plain flags.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };
}

/// Raises a noncontinuable access violation through Windows. Nothing in Rust handles it, so the
/// process ends the way a native crash ends it.
#[cfg(debug_assertions)]
pub fn raise_access_violation() -> ! {
    // SAFETY: raises an exception with no arguments; no pointer is passed.
    unsafe {
        RaiseException(
            EXCEPTION_ACCESS_VIOLATION.cast_unsigned(),
            EXCEPTION_NONCONTINUABLE,
            0,
            std::ptr::null(),
        );
    }
    std::process::abort()
}

/// Loads the fixture DLL at `path` and calls its `kumochi_test_throw`, which throws a C++
/// exception.
///
/// # Errors
/// Fails when the DLL doesn't load or lacks the export; otherwise it doesn't return.
#[cfg(debug_assertions)]
pub(crate) fn call_throwing_export(path: &Path) -> io::Result<()> {
    let library = Library::load(path)?;
    let export = library
        .export(c"kumochi_test_throw")
        .ok_or(io::ErrorKind::NotFound)?;
    // SAFETY: the fixture declares `extern "C" void kumochi_test_throw(void)` and is built with
    // `/EHs`, so it may throw; `C-unwind` makes that a defined foreign unwind into Rust.
    let throw = unsafe { std::mem::transmute::<RawExport, unsafe extern "C-unwind" fn()>(export) };
    // SAFETY: as above; it takes nothing and the library stays loaded for the call.
    unsafe { throw() };
    drop(library);
    Ok(())
}
