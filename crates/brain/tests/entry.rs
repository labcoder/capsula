//! The unsupported executable modes must refuse before reaching app data.
#![cfg(not(windows))]

#[path = "../../../tests/support/scratch.rs"]
mod scratch;

use std::process::Command;

use kumochi_brain::{MCP_BRIDGE, SCREEN_WORKER, SPEECH_WORKER};
use kumochi_protocol::constants::{
    CATALOG_PROVENANCE_ARG, DRAW_CHECK_ARG, IMAGE_CHECK_ARG, IMAGE_DIAGNOSTIC_ARG, MARKS_CHECK_ARG,
};

#[test]
fn native_executable_modes_refuse_without_touching_app_data() {
    let dir = scratch::dir("unsupported-entry");
    for args in [
        &[][..],
        &[MCP_BRIDGE, "0123456789abcdef0123456789abcdef"],
        &[SCREEN_WORKER],
        &[SPEECH_WORKER],
        &[IMAGE_DIAGNOSTIC_ARG],
        &[IMAGE_CHECK_ARG],
        &[MARKS_CHECK_ARG],
        &[DRAW_CHECK_ARG],
        &[CATALOG_PROVENANCE_ARG],
    ] {
        let child = Command::new(env!("CARGO_BIN_EXE_kumochid"))
            .args(args)
            .env_clear()
            .env("HOME", dir.join("home"))
            .env("LOCALAPPDATA", dir.join("local"))
            .env("APPDATA", dir.join("roaming"))
            .current_dir(dir.path())
            .output()
            .expect("run the test build of the brain");
        assert_eq!(child.status.code(), Some(69), "{args:?}");
        assert!(child.stdout.is_empty(), "{args:?}");
        assert!(child.stderr.is_empty(), "{args:?}");
        assert_eq!(
            dir.read_dir().expect("scratch directory").count(),
            0,
            "{args:?}"
        );
    }
}
