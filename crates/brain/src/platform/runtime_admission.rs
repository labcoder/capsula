use std::io;

/// Checks whether this build includes a native runtime adapter, without doing I/O.
///
/// # Errors
/// Returns `Unsupported` on builds without native runtime composition.
pub fn require_native_runtime() -> io::Result<()> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err(io::ErrorKind::Unsupported.into())
    }
}
