//! The unsupported executable modes must refuse before reaching app data.
#![cfg(not(windows))]

#[path = "../../../tests/support/scratch.rs"]
#[expect(
    clippy::print_stderr,
    reason = "the shared fixture reports cleanup failures during unwind"
)]
mod scratch;

use std::process::Command;

use kumochi_brain::{MCP_BRIDGE, PARENT_BOUND, SCREEN_WORKER, SPEECH_WORKER};
use kumochi_protocol::constants::{
    CATALOG_PROVENANCE_ARG, DRAW_CHECK_ARG, IMAGE_CHECK_ARG, IMAGE_DIAGNOSTIC_ARG, MARKS_CHECK_ARG,
};

#[test]
fn native_executable_modes_refuse_without_touching_app_data() {
    let dir = scratch::dir("unsupported-entry");
    for (args, code) in [
        (&[][..], 69),
        (&[MCP_BRIDGE, "0123456789abcdef0123456789abcdef"], 69),
        (&[SCREEN_WORKER], 69),
        (&[SPEECH_WORKER], 69),
        (&[IMAGE_DIAGNOSTIC_ARG], 69),
        (&[IMAGE_CHECK_ARG], 69),
        (&[MARKS_CHECK_ARG], 69),
        (&[DRAW_CHECK_ARG], 69),
        (&[CATALOG_PROVENANCE_ARG], 69),
        (&[PARENT_BOUND], 69),
        (&[PARENT_BOUND, ""], 64),
        (&[PARENT_BOUND, SCREEN_WORKER], 64),
    ] {
        #[expect(
            clippy::disallowed_methods,
            reason = "this non-Windows test checks the real executable without inheritable Windows handles"
        )]
        let child = Command::new(env!("CARGO_BIN_EXE_kumochid"))
            .args(args)
            .env_clear()
            .env("HOME", dir.join("home"))
            .env("LOCALAPPDATA", dir.join("local"))
            .env("APPDATA", dir.join("roaming"))
            .current_dir(dir.path())
            .output()
            .expect("run the test build of the brain");
        assert_eq!(child.status.code(), Some(code), "{args:?}");
        assert!(child.stdout.is_empty(), "{args:?}");
        assert!(child.stderr.is_empty(), "{args:?}");
        assert_eq!(
            dir.read_dir().expect("scratch directory").count(),
            0,
            "{args:?}"
        );
    }
}
