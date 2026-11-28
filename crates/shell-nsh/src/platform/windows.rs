//! The shell on Windows: the GPUI shell's start-up, then a `NativeShell` app with one hidden main
//! window and no view that quits on its deadline.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use kumochi_protocol::constants;
use kumochi_shell_core::instance::InstanceLock;
use kumochi_shell_core::logging;
use kumochi_shell_core::startup::{self, Refused};
use nsh_core::desktop::MainWindowOptions;
use nsh_core::element::desktop::quit;
use nsh_core::element::{Component, IntoElement, NoEvent, Props, Scope, spawn, timer, view};
use nsh_platform_windows::{AppOptions, AppSource, CacheOptions, Root, run_app};

use crate::launch::{Launch, LaunchError};

/// The root's one prop: the milliseconds left of the deadline when the engine mounts the root,
/// since props take numbers, not an `Instant`. The view's timer starts one commit later.
#[derive(Clone, PartialEq, Props)]
struct EmptyAppProps {
    quit_after_ms: u64,
}

/// An app with nothing to show yet. Its one task quits the app when the deadline passes.
struct EmptyApp;

impl Component for EmptyApp {
    type Props = EmptyAppProps;
    type Event = NoEvent;

    fn view(props: EmptyAppProps, _: Scope<NoEvent>) -> impl IntoElement {
        spawn(async move {
            timer(Duration::from_millis(props.quit_after_ms)).await;
            quit();
        });
        view()
    }
}

/// The GPUI shell's own start: the same lock and log, so the two shells exclude each other.
pub(crate) fn start(
    launch: Result<Launch, LaunchError>,
) -> Result<(Launch, InstanceLock), ExitCode> {
    startup::start(InstanceLock::admit_current_session, launch, || {
        logging::init(constants::SHELL_LOG)
    })
    .map_err(Refused::exit_code)
}

/// Runs the app until it quits at `deadline`, then ends the process: 0 when its message loop
/// ended with 0, else 1.
pub(crate) fn run(deadline: Instant) -> ExitCode {
    let mut window = MainWindowOptions::default();
    window.default_visible = Some(false);
    // The engine makes the props as it mounts the root, which can come late: only what's left then,
    // so the quit can trail the deadline by the one commit before the view's timer starts.
    let root = Root::new::<EmptyApp>(move || {
        let left = deadline.saturating_duration_since(Instant::now());
        EmptyAppProps {
            quit_after_ms: u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
        }
    })
    .app_id(constants::SHELL_APP_ID)
    .main_window(window);
    // shortcut: NativeShell's own `log` lines go nowhere, since their content (paths, foreign
    // error text) isn't vetted for Kumochi's log; bridge them through an allow-list of targets
    // once the shell shows something worth diagnosing.
    run_app(
        AppSource::Rust(root),
        AppOptions {
            app_id: constants::SHELL_APP_ID.to_owned(),
            title: constants::DISPLAY_NAME.to_owned(),
            skip_taskbar: true,
            cache: CacheOptions::off(),
            ..AppOptions::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use super::*;

    #[test]
    fn the_binary_has_the_name_the_staging_task_copies() {
        assert_eq!(
            concat!(env!("CARGO_BIN_NAME"), ".exe"),
            constants::NSH_SHELL_EXE
        );
    }

    #[test]
    fn an_argument_that_isnt_unicode_is_unknown() {
        // The flag with a valid delay, then an unpaired surrogate: a Windows argument, not Unicode.
        let mut arg: Vec<u16> = "--quit-after-ms=2000".encode_utf16().collect();
        arg.push(0xD800);
        assert_eq!(
            Launch::parse(std::iter::once(OsString::from_wide(&arg))),
            Err(LaunchError::Unknown)
        );
    }
}
