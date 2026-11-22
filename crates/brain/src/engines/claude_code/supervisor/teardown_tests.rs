//! Teardown regressions, beside `tests.rs` and built on its fixtures.

use std::fs;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use kumochi_protocol::constants::ENGINE_DIR;

use super::tests::{WAIT, end, found, launch_dirs, pipes, question, queue};
use super::{Teardown, admit};
use crate::engines::claude_code::lane::tests::{Base, locked};
use crate::platform::pause_after_check;

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
    let file = work.join("held");
    fs::write(&file, b"").expect("a file in the launch folder");
    let held = locked(&file);
    let (released_tx, released) = std_mpsc::channel();
    let again = work.clone();
    // A second walk of the folder means the first attempt failed; only then does the hold go,
    // and the signal says the held file outlived that attempt.
    pause_after_check(
        work,
        Box::new(move || {
            pause_after_check(
                again,
                Box::new(move || {
                    let outlived = file.exists();
                    drop(held);
                    released_tx.send(outlived).ok();
                }),
            );
        }),
    );
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(
        released.try_recv(),
        Ok(true),
        "a retry found the file the first attempt couldn't delete, then released it"
    );
    assert_eq!(launch_dirs(&base), Vec::<String>::new());
}
