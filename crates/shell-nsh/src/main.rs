//! `kumochi-nsh`, Kumochi's `NativeShell` shell. It has no console window. It starts like the GPUI
//! shell, holds the same single-instance lock and log, and runs an empty, hidden app until its
//! `--quit-after-ms` deadline.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod launch;
mod platform;

use std::process::ExitCode;
use std::time::Duration;

use kumochi_protocol::constants::{self, ProductIdentity};

use crate::launch::Launch;

/// The process exit code for `identity`: 0 for Dev and Gate, else the brain's 69 for unsupported.
fn exit_code(identity: ProductIdentity) -> u8 {
    // The refusal goes away when this shell replaces the GPUI one.
    if identity == ProductIdentity::DEV || identity == ProductIdentity::GATE {
        0
    } else {
        69
    }
}

/// How long past its deadline the shell may run before the backstop ends it: longer than the pin's
/// 10 s wait for a process it replaces plus its 200 ms of quit hooks.
// shortcut: unmeasured; set it from the native run's start-to-exit times.
const BACKSTOP_GRACE: Duration = Duration::from_secs(15);

/// Runs `expire` on a thread of its own once `after` has passed, whatever `NativeShell` is doing.
fn arm_backstop(after: Duration, expire: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("kumochi-backstop".to_owned())
        .spawn(move || {
            std::thread::sleep(after);
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
    tracing::info!(
        build = kumochi_protocol::BUILD_ID,
        identity = constants::DATA_DIR,
        edition = constants::EDITION,
        "shell started"
    );
    // NativeShell usually ends the process itself, so the lock is held until it does.
    let _lock = lock;
    // NativeShell can keep running without the engine that holds the deadline, or wait on a
    // message box; a hidden shell must never keep the session's lock forever.
    let armed = arm_backstop(launch.quit_after + BACKSTOP_GRACE, || {
        tracing::error!("the shell outlived its deadline; ending it");
        std::process::exit(1);
    });
    if armed.is_err() {
        tracing::error!("can't start the deadline backstop");
        return ExitCode::FAILURE;
    }
    platform::run(launch.quit_after)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, RecvTimeoutError};

    use super::*;

    #[test]
    fn the_backstop_expires_after_its_delay() {
        let (expired, heard) = mpsc::channel();
        arm_backstop(Duration::from_millis(5), move || {
            expired.send(()).expect("the test is listening");
        })
        .expect("the backstop starts");
        assert_eq!(heard.recv_timeout(Duration::from_secs(10)), Ok(()));
    }

    #[test]
    fn the_backstop_waits_for_its_delay() {
        let (expired, heard) = mpsc::channel();
        arm_backstop(Duration::from_secs(10), move || {
            expired.send(()).ok();
        })
        .expect("the backstop starts");
        assert_eq!(
            heard.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
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
