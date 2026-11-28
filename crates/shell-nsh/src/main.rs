//! `kumochi-nsh`, Kumochi's `NativeShell` shell. It has no console window and starts nothing yet.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::process::ExitCode;

use kumochi_protocol::constants::ProductIdentity;

/// The exit code of a build whose identity this shell refuses, the brain's 69 for unsupported.
const UNSUPPORTED: u8 = 69;

// The refusal goes away when this shell replaces the GPUI one.
fn admits(identity: ProductIdentity) -> bool {
    identity == ProductIdentity::DEV || identity == ProductIdentity::GATE
}

fn main() -> ExitCode {
    if admits(ProductIdentity::ACTIVE) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(UNSUPPORTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_dev_and_gate_identities_are_admitted() {
        assert!(!admits(ProductIdentity::RELEASE), "release is refused");
        assert!(admits(ProductIdentity::DEV), "dev is admitted");
        assert!(admits(ProductIdentity::GATE), "gate is admitted");
    }
}
