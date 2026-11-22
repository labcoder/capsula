//! Teardown regressions, beside `tests.rs` and built on its fixtures.

use std::fs;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use kumochi_protocol::constants::ENGINE_DIR;

use super::tests::{WAIT, end, found, launch_dirs, pipes, question, queue};
use super::{Teardown, admit};
use crate::engines::claude_code::lane::tests::{Base, locked};

/// OSC-740: a handle another process of the launch closes after the first delete attempt, as the
/// bridge may after Claude Code exits, no longer leaves the folder for the sweep.
#[tokio::test]
async fn a_folder_freed_during_teardown_is_still_deleted() {
    let base = Base::new("launch-folder-freed");
    let lane = found(&base, "deaf").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (id_tx, id) = std_mpsc::channel();
    start.hooks.id = Some(id_tx);
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    let id = id.recv_timeout(WAIT).expect("the id");
    let work = base.owned().join(ENGINE_DIR).join(id.hex()).join("work");
    // Listed before the held file, so the first attempt deletes it and then stops at the lock.
    let marker = work.join("a-marker");
    fs::write(&marker, b"").expect("a marker in the launch folder");
    let file = work.join("held");
    fs::write(&file, b"").expect("a file in the launch folder");
    let held = locked(&file);
    let release = std::thread::spawn(move || {
        let until = Instant::now() + WAIT;
        while marker.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(held);
    });
    assert_eq!(end(launch).await, Teardown::Done);
    release.join().expect("the lock is released");
    assert_eq!(launch_dirs(&base), Vec::<String>::new());
}
