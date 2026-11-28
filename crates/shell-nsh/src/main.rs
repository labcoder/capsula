//! `kumochi-nsh`, Kumochi's `NativeShell` shell. It has no console window and starts nothing yet.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::process::ExitCode;

use kumochi_protocol::constants::ProductIdentity;

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
    ExitCode::from(exit_code(ProductIdentity::ACTIVE))
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
