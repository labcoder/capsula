use std::io;

/// The brain's launch and lifetime contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The brain runs independently of the shell that starts it.
    Detached,
    /// The shell starts the brain with an inherited pair and owns its lifetime.
    ParentBound,
}

/// Whether this build has the native adapters for the requested mode.
#[must_use]
pub const fn supports(mode: Mode) -> bool {
    match mode {
        Mode::Detached => cfg!(windows),
        Mode::ParentBound => false,
    }
}

pub(crate) fn require_mode(mode: Mode) -> io::Result<()> {
    if supports(mode) {
        Ok(())
    } else {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(windows)]
pub(crate) use crate::speech::worker::serve as serve_speech;

#[cfg(not(windows))]
pub(crate) fn serve_speech() -> Result<(), kumochi_protocol::frame::FrameError> {
    Err(io::Error::from(io::ErrorKind::Unsupported).into())
}
