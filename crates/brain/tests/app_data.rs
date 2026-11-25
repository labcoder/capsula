//! A debug brain refuses an override profile reached through a link before its first write. Its
//! own binary, because a child started here while another test spawned a worker could inherit
//! that worker's pipe ends.
#![cfg(all(windows, debug_assertions))]

#[cfg(test)]
#[path = "../../../tests/support/scratch.rs"]
#[expect(dead_code, reason = "the shared fixture serves several test binaries")]
mod scratch;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{LOCAL_APPDATA_OVERRIDE, ROAMING_APPDATA_OVERRIDE};
use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

/// Startup refuses within milliseconds; a brain that got past it waits for a shell instead.
const PATIENCE: Duration = Duration::from_secs(30);

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the only test this binary runs, so no spawn overlaps it"
)]
fn a_linked_override_root_stops_the_brain_before_it_writes() {
    let local = kumochi_storage::known_folder(FOLDERID_LocalAppData).expect("local app data");
    let dir = scratch::Scratch::under(&local, "brain-linked-root");
    let (linked, roaming, target) = (dir.join("local"), dir.join("roaming"), dir.join("target"));
    for folder in [&linked, &roaming, &target] {
        std::fs::create_dir(folder).expect("a scratch folder");
    }
    kumochi_storage::set_junction(&linked, &target).expect("a junction to the target");
    let mut brain = Command::new(env!("CARGO_BIN_EXE_kumochid"))
        .env_clear()
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").expect("system root"),
        )
        .env(LOCAL_APPDATA_OVERRIDE, &linked)
        .env(ROAMING_APPDATA_OVERRIDE, &roaming)
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the test build of the brain");
    let until = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = brain.try_wait().expect("the brain's status") {
            break status;
        }
        if Instant::now() >= until {
            brain.kill().expect("end the brain that started");
            brain.wait().expect("reap it");
            panic!("the brain started through a linked override root");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
    for folder in [&target, &roaming] {
        assert_eq!(
            std::fs::read_dir(folder).expect("the folder lists").count(),
            0,
            "nothing was written to {}",
            folder.display()
        );
    }
}
