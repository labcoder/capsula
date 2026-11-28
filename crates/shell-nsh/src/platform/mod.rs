//! What differs by platform: how the shell starts and the app it runs.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::{run, start};

/// Off Windows there is no `NativeShell` backend here yet: the brain's 69 for unsupported, before
/// anything is admitted, opened or logged.
#[cfg(not(windows))]
pub(crate) fn start(
    _launch: Result<crate::launch::Launch, crate::launch::LaunchError>,
) -> Result<
    (
        crate::launch::Launch,
        kumochi_shell_core::instance::InstanceLock,
    ),
    std::process::ExitCode,
> {
    Err(std::process::ExitCode::from(crate::UNSUPPORTED))
}

/// Never reached off Windows, since [`start`] refuses first; it answers unsupported too.
#[cfg(not(windows))]
pub(crate) fn run(_quit_after: std::time::Duration) -> std::process::ExitCode {
    std::process::ExitCode::from(crate::UNSUPPORTED)
}
