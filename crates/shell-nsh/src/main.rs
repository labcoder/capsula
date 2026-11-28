//! `kumochi-nsh`, Kumochi's `NativeShell` shell. It has no console window. It starts like the GPUI
//! shell, holds the same single-instance lock and log, and runs an empty, hidden app until its
//! `--quit-after-ms` deadline.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod launch;
mod platform;

use std::process::ExitCode;

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
    // NativeShell ends the process itself, so the lock is held until the OS closes it at exit.
    let _lock = lock;
    platform::run(launch.quit_after)
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
