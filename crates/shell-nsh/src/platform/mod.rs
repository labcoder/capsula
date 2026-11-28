//! What differs by platform: how the shell starts and the app it runs. The backstop and the
//! link's reducer are shared code, declared here because only the Windows app uses all of them
//! yet, and the OS conditions that say so belong here.

#[cfg_attr(
    all(not(windows), not(test)),
    expect(
        dead_code,
        reason = "only the Windows app tells the backstop of a connection or a quit"
    )
)]
#[path = "../backstop.rs"]
pub(crate) mod backstop;
#[cfg_attr(
    not(windows),
    expect(dead_code, reason = "only the Windows app follows the link yet")
)]
#[path = "../shell.rs"]
pub(crate) mod shell;
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
pub(crate) fn run(
    _deadline: Option<std::time::Instant>,
    _backstop: backstop::Backstop,
) -> std::process::ExitCode {
    std::process::ExitCode::from(crate::UNSUPPORTED)
}
