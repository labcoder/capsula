//! Thread ops over the pipes: a task's rename and model reach the next page, and a rename of
//! the main thread changes nothing and never shows its title in the logs or the tap.

use std::fs::File;

use kumochi_protocol::constants::CONFIG_FILE;
use kumochi_protocol::message::{
    Effort, ModelChoice, ModelSlug, PageOf, ThreadKind, ThreadOp, ThreadTitle,
};
use kumochi_protocol::tap;

use super::*;
use crate::runtime::Seen;
use crate::store::RunEnd;
use crate::test_support::{Captured, SENTINEL};

fn rename(thread: ThreadId, title: &str) -> ControlToBrain {
    ControlToBrain::ThreadOp(ThreadOp::Rename {
        thread,
        title: ThreadTitle::new(title).expect("valid"),
    })
}

#[tokio::test]
async fn a_tasks_rename_and_model_reach_the_list_and_kumos_rename_changes_nothing() {
    let scratch = Scratch::new("thread-ops");
    let (task, main) = (ThreadId(1), ThreadId(2));
    {
        // A task from before the main thread existed, as after an upgrade.
        let mut store = Store::open(&scratch.db()).expect("store");
        store
            .begin_turn(task, Some("first words"), RunId(1), "hi")
            .expect("turn");
        store
            .end_run(task, RunId(1), &[], RunEnd::Done)
            .expect("ended");
    }
    let config = scratch.db().with_file_name(CONFIG_FILE);
    std::fs::write(&config, "assistant_name = \"Nimbus\"\n").expect("settings");
    let saved = std::fs::read(&config).expect("settings");
    let captured = Captured::default();
    let _logs = captured.install();
    let mut brain = test_support::brain(&scratch, "");
    let mut seen = brain.observe();
    let recording = scratch.db().with_file_name("tap.bin");
    let mut tap = Tap::to_file(File::create(&recording).expect("tap file"));
    let ((control, data), mut shell) = duplex();
    let choice = ModelChoice {
        model: ModelSlug::new("gpt-6.1-sol").expect("valid"),
        effort: Effort::High,
    };
    let client = async {
        shell.scenes().await;
        assert_eq!(shell.views[0].main_thread, main);
        let ops = [
            rename(main, SENTINEL),
            rename(task, "Renamed task"),
            ControlToBrain::ThreadOp(ThreadOp::SetModel {
                thread: task,
                choice: choice.clone(),
            }),
        ];
        for op in &ops {
            shell.send(op).await;
            timeout(PATIENCE, async {
                while seen.recv().await != Some(Seen::ControlApplied) {}
            })
            .await
            .expect("applied");
        }
        let page = DataToBrain::PageRequest {
            request: 1,
            page: PageOf::Threads {
                search: None,
                after: None,
            },
        };
        let frame = frame::encode(&page, Pipe::Data).expect("encodes");
        shell.data_out.write_all(&frame).await.expect("sent");
        let reply = shell.data().await;
        drop(shell);
        reply
    };
    let (ended, reply) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    drop(tap);
    let DataToShell::ThreadList { threads, next, .. } = reply else {
        panic!("a list, got {reply:?}")
    };
    let rows: Vec<_> = threads
        .iter()
        .map(|row| (row.id, row.title.as_str(), row.kind, row.choice.clone()))
        .collect();
    assert_eq!(
        rows,
        [
            (main, "", ThreadKind::Main, None),
            (task, "Renamed task", ThreadKind::Thread, Some(choice)),
        ]
    );
    assert_eq!(next, None);
    assert_eq!(std::fs::read(&config).expect("settings"), saved);
    let logs = captured.text();
    assert!(logs.contains("a rename of the main thread was refused"), "{logs}");
    assert!(!logs.contains(SENTINEL), "{logs}");
    let recorded = std::fs::read(&recording).expect("tap read");
    assert!(
        !recorded.windows(SENTINEL.len()).any(|w| w == SENTINEL.as_bytes()),
        "the title was recorded"
    );
    let mut dumped = Vec::new();
    tap::dump(&mut recorded.as_slice(), &mut dumped).expect("dumps");
    let dumped = String::from_utf8(dumped).expect("utf-8");
    assert_eq!(dumped.matches("shell -> brain  ThreadOp (").count(), 3, "{dumped}");
}
