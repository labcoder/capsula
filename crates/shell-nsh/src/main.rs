//! `kumochi-nsh`, Kumochi's `NativeShell` shell. It has no console window. It starts like the GPUI
//! shell, holds the same single-instance lock and log, and runs an empty, hidden app until its
//! `--quit-after-ms` deadline.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod launch;
mod platform;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{self, ProductIdentity};

use crate::launch::Launch;

/// The exit code for a build or system this shell doesn't run on, as the brain's.
const UNSUPPORTED: u8 = 69;

/// The exit code when the deadline backstop ends the shell, as the `timeout` command's. Nothing
/// else here or in `run_app` exits with it, so it shows the backstop fired without a log write.
const OVERDUE: u8 = 124;

/// The process exit code for `identity`: 0 for Dev and Gate, else the brain's 69 for unsupported.
fn exit_code(identity: ProductIdentity) -> u8 {
    // The refusal goes away when this shell replaces the GPUI one.
    if identity == ProductIdentity::DEV || identity == ProductIdentity::GATE {
        0
    } else {
        UNSUPPORTED
    }
}

/// How long past its deadline the shell is allowed before the backstop ends it: a budget chosen above
/// the pin's 10 s wait for a process it replaces and its 200 ms of quit hooks, not a bound, since
/// window creation, mounting and the engine's join have no limit of their own.
// shortcut: unmeasured, so a slow but healthy start can end with 124; set it from the native run.
const BACKSTOP_GRACE: Duration = Duration::from_secs(15);

/// Runs `expire` on a thread of its own once `at` has passed, whatever `NativeShell` is doing.
fn arm_backstop(at: Instant, expire: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("shell-backstop".to_owned())
        .spawn(move || {
            std::thread::sleep(at.saturating_duration_since(Instant::now()));
            expire();
        })
        .map(drop)
}

fn main() -> ExitCode {
    let refused = exit_code(ProductIdentity::ACTIVE);
    if refused != 0 {
        return ExitCode::from(refused);
    }
    let launch = Launch::parse(std::env::args_os().skip(1));
    let (launch, lock) = match platform::start(launch) {
        Ok(started) => started,
        Err(code) => return code,
    };
    // The deadline, from taking the lock: the backstop fires at a fixed time past it, and the app
    // waits what's left of it when it mounts.
    let deadline = Instant::now() + launch.quit_after;
    // NativeShell can keep running without the engine that holds the deadline, or wait on a
    // message box, and a log write can block: a hidden shell must never keep the session's lock
    // forever, so nothing comes between taking the lock and arming.
    let armed = arm_backstop(deadline + BACKSTOP_GRACE, || {
        // `run_app` ends a run that got its UI thread this way too: `std::process::exit` off it.
        std::process::exit(i32::from(OVERDUE))
    });
    if armed.is_err() {
        drop(lock);
        tracing::error!("can't start the deadline backstop");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        build = kumochi_protocol::BUILD_ID,
        identity = constants::DATA_DIR,
        edition = constants::EDITION,
        "shell started"
    );
    // NativeShell usually ends the process itself, so the lock is held until it does.
    let _lock = lock;
    platform::run(deadline)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    #[test]
    fn the_backstop_runs_its_action_once_its_delay_has_passed() {
        let (expired, heard) = mpsc::channel();
        let at = Instant::now() + Duration::from_millis(50);
        arm_backstop(at, move || {
            expired.send(Instant::now()).expect("the test is listening");
        })
        .expect("the backstop starts");
        let ran = heard
            .recv_timeout(Duration::from_secs(10))
            .expect("the action runs");
        assert!(ran >= at, "it ran {:?} early", at - ran);
    }

    #[test]
    fn only_the_dev_and_gate_identities_exit_successfully() {
        assert_eq!(
            exit_code(ProductIdentity::RELEASE),
            69,
            "release is refused"
        );
        assert_eq!(exit_code(ProductIdentity::DEV), 0, "dev is admitted");
        assert_eq!(exit_code(ProductIdentity::GATE), 0, "gate is admitted");
    }
}
