//! `kumochi-nsh`, Kumochi's `NativeShell` shell. It has no console window. It starts like the GPUI
//! shell, holds the same single-instance lock and log, links to its brain and follows the link's
//! state in an empty, hidden app until the link ends, a quit or its `--quit-after-ms` deadline.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod launch;
mod platform;

use std::process::ExitCode;
use std::time::Instant;

use kumochi_protocol::constants::{self, ProductIdentity};

use crate::launch::Launch;
use crate::platform::backstop::{Backstop, GRACE, Limit, START_LIMIT};

/// The exit code for a build or system this shell doesn't run on, as the brain's.
const UNSUPPORTED: u8 = 69;

/// The exit code when the backstop ends the shell, as the `timeout` command's. Nothing else here
/// or in `run_app` exits with it, so it shows the backstop fired without a log write.
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
    let now = Instant::now();
    let deadline = launch.quit_after.map(|after| now + after);
    // NativeShell can keep running without the engine that runs the link, or wait on a message
    // box, and a log write can block: a hidden shell must never keep the session's lock forever,
    // so nothing comes between taking the lock and arming.
    let limit = Limit::at_start(now, launch.quit_after);
    let armed = Backstop::arm(limit, GRACE, START_LIMIT, || {
        // `run_app` ends a run that got its UI thread this way too: `std::process::exit` off it.
        std::process::exit(i32::from(OVERDUE))
    });
    let Ok(backstop) = armed else {
        drop(lock);
        tracing::error!("can't start the deadline backstop");
        return ExitCode::FAILURE;
    };
    tracing::info!(
        build = kumochi_protocol::BUILD_ID,
        identity = constants::DATA_DIR,
        edition = constants::EDITION,
        "shell started"
    );
    // NativeShell usually ends the process itself, so the lock is held until it does.
    let _lock = lock;
    platform::run(deadline, backstop)
}

#[cfg(test)]
mod tests {
    use super::*;

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
