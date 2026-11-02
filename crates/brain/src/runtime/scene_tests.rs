//! Scene recovery over a shell connection: the connect barrier, clears, Quit and privacy.

use std::fs::File;
use std::time::Duration;

use capsula_protocol::frame::{self, Pipe};
use capsula_protocol::message::{
    ControlToBrain, DataToShell, Phase, RunId, SceneDescriptor, SceneId, Step, ThreadId,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::time::timeout;

#[cfg(windows)]
use super::pipes;
use super::{PATIENCE, Shell, duplex, hold, locked_brain, outcome, outcomes, patient_brain};
use crate::coordinator::Input;
use crate::runtime::{Brain, CLOSE_WAIT, Ended, Quit, SCENES_WAIT};
use crate::store::{RunEnd, SceneTarget, Store};
use crate::tap::Tap;
use crate::test_support::{self, Captured, SENTINEL, Scratch};

/// The first step's write starts this long after the run's text, so a test can hold the store
/// before it.
const SCRIPT: &str = "0 text a\n300 step one\n5000 text never";
/// Long enough for that write to have reached the store and be waiting on the lock.
const STEP_WRITTEN: Duration = Duration::from_millis(800);

/// Scenes drawn before the brain starts, each by its own thread's run from `labels`. A fifth
/// evicts the oldest, as the brain does.
fn drawn(scratch: &Scratch, scenes: &[&[&str]]) -> Vec<SceneId> {
    let mut store = Store::open(&scratch.db()).expect("store");
    let mut ids: Vec<SceneId> = Vec::new();
    for labels in scenes {
        let (thread, run) = store.high_water().expect("high water");
        let (thread, run) = (ThreadId(thread.0 + 1), RunId(run.0 + 1));
        store
            .begin_turn(thread, Some("title"), run, "turn")
            .expect("turn");
        let evict = (ids.len() >= 4).then(|| ids[ids.len() - 4]);
        let drawing = (thread, run);
        let scene = store
            .commit_step(drawing, None, SceneTarget::New { evict }, 0, labels[0])
            .expect("first step");
        for (index, label) in (1..).zip(&labels[1..]) {
            store
                .commit_step(drawing, None, SceneTarget::Existing(scene), index, label)
                .expect("step");
        }
        store
            .end_run(thread, run, &[], RunEnd::Done)
            .expect("ended");
        ids.push(scene);
    }
    ids
}

/// The scenes the next start recovers, oldest first.
fn recovered(scratch: &Scratch) -> Vec<SceneId> {
    let store = Store::open(&scratch.db()).expect("reopen");
    let scenes = store.scenes().expect("scenes");
    scenes.iter().map(|state| state.scene).collect()
}

fn ids(scenes: &[SceneDescriptor]) -> Vec<SceneId> {
    scenes.iter().map(|named| named.scene).collect()
}

fn named(scene: SceneId, steps: u32) -> SceneDescriptor {
    SceneDescriptor {
        scene,
        revision: steps,
        committed: steps,
    }
}

fn clear(scenes: &[SceneId]) -> ControlToBrain {
    ControlToBrain::ClearDrawing {
        scene_ids: scenes.to_vec(),
    }
}

/// Submits a turn, waits for its text, then holds the store, so the run's first step waits on
/// the lock once it's written. Returns the run and the lock.
async fn hold_first_step<S: AsyncRead + AsyncWrite>(
    shell: &mut Shell<S>,
    scratch: &Scratch,
) -> (RunId, rusqlite::Connection) {
    shell.submit("go").await;
    let (run, _) = shell.state().await;
    loop {
        match shell.data().await {
            DataToShell::AssistantText { .. } => break,
            DataToShell::SceneSnapshot { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    let lock = hold(scratch);
    tokio::time::sleep(STEP_WRITTEN).await;
    (run, lock)
}

/// Sends a turn and waits for its run state. The core takes controls before turns, so every
/// control sent before it has been applied.
async fn settle<S: AsyncRead + AsyncWrite>(shell: &mut Shell<S>) {
    shell.submit("settle").await;
    shell.state().await;
}

/// Serves one connection whose shell only takes `Scenes`, then leaves.
async fn scenes_on_reconnect(brain: &mut Brain) -> Vec<SceneDescriptor> {
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let scenes = shell.scenes().await;
        drop(shell);
        scenes
    };
    let (ended, scenes) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    scenes
}

/// Serves a connection whose shell sends `controls` and then quits, leaving at once.
async fn quit_with(brain: &mut Brain, controls: &[ControlToBrain]) -> Ended {
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.scenes().await;
        for message in controls {
            shell.send(message).await;
        }
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
    };
    tokio::join!(brain.serve(control, data, &mut tap), client).0
}

fn deadline(ended: Ended) -> tokio::time::Instant {
    let Ended::Quit { deadline } = ended else {
        panic!("expected a quit, got {ended:?}");
    };
    deadline
}

/// Releases `lock` after `after`, from its own thread.
fn release_after(lock: rusqlite::Connection, after: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        lock.execute_batch("ROLLBACK").expect("unlocked");
    })
}

#[tokio::test]
async fn a_held_first_step_holds_back_scenes_and_turns_and_the_connection_ends_unavailable() {
    let scratch = Scratch::new("barrier-held");
    let mut brain = patient_brain(&scratch, SCRIPT);
    let mut tap = Tap::from_env();
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        let (_, lock) = hold_first_step(&mut shell, &scratch).await;
        drop(shell);
        lock
    };
    let (ended, lock) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let ((control, data), mut shell) = duplex();
    let started = tokio::time::Instant::now();
    let client = async {
        shell.submit("not taken").await;
        assert_eq!(
            shell.control().await,
            None,
            "neither Scenes nor a run state"
        );
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Unavailable);
    assert!(started.elapsed() >= SCENES_WAIT);
    lock.execute_batch("ROLLBACK").expect("unlocked");
    // The shell kept its clear intent across the loss and spends it on the complete list.
    let ((control, data), mut shell) = duplex();
    let client = async {
        let scenes = shell.scenes().await;
        shell.send(&clear(&ids(&scenes))).await;
        settle(&mut shell).await;
        drop(shell);
        scenes
    };
    let (ended, scenes) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    // The turn, the text, then the scene's `scene_begin`.
    assert_eq!(scenes, [named(SceneId(3), 1)]);
    brain.close().await;
    assert_eq!(
        outcomes(&scratch),
        [
            outcome("interrupted", "shell_lost"),
            outcome("interrupted", "shell_lost")
        ],
        "the turn sent while unavailable was never taken"
    );
    assert_eq!(recovered(&scratch), []);
}

/// Four scenes, then a fifth whose first step, which evicts the oldest, waits on the lock as
/// its shell leaves.
async fn held_fifth_scene(scratch: &Scratch) -> (Brain, Vec<SceneId>, rusqlite::Connection) {
    let four = drawn(scratch, &[&["a"], &["b"], &["c"], &["d"]]);
    let mut brain = patient_brain(scratch, SCRIPT);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(ids(&shell.scenes().await), four);
        let (_, lock) = hold_first_step(&mut shell, scratch).await;
        drop(shell);
        lock
    };
    let (ended, lock) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    (brain, four, lock)
}

#[tokio::test]
async fn scenes_after_a_held_fifth_scene_name_the_four_its_transaction_committed() {
    let scratch = Scratch::new("barrier-fifth");
    let (mut brain, four, lock) = held_fifth_scene(&scratch).await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        lock.execute_batch("ROLLBACK").expect("unlocked");
        let scenes = shell.scenes().await;
        drop(shell);
        scenes
    };
    let (ended, scenes) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let named = ids(&scenes);
    assert_eq!(named[..3], four[1..], "the oldest was evicted");
    assert_eq!(named.len(), 4);
    assert!(named[3] > four[3], "the new scene");
    brain.close().await;
    assert_eq!(recovered(&scratch), named);
}

#[tokio::test]
async fn stop_and_clear_are_served_while_the_barrier_waits() {
    let scratch = Scratch::new("barrier-served");
    let (mut brain, four, lock) = held_fifth_scene(&scratch).await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.stop(RunId(5)).await;
        shell.send(&clear(&[four[1]])).await;
        // Nothing answers either, so give the core a moment to take them before the store frees.
        tokio::time::sleep(Duration::from_millis(200)).await;
        lock.execute_batch("ROLLBACK").expect("unlocked");
        let scenes = shell.scenes().await;
        drop(shell);
        scenes
    };
    let (ended, scenes) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let named = ids(&scenes);
    assert_eq!(
        named[..2],
        four[2..],
        "B was cleared while the barrier waited"
    );
    assert_eq!(named.len(), 3);
    brain.close().await;
    assert_eq!(recovered(&scratch), named);
}

/// A scene left with nothing kept must leave the set durably too: otherwise it would still hold
/// one of the newest four places at the next start, and push a visible scene out.
#[tokio::test]
async fn a_stop_that_kept_nothing_brings_no_scene_back_once_its_late_ack_lands() {
    let scratch = Scratch::new("kept-nothing");
    let three = drawn(&scratch, &[&["a"], &["b"], &["c"]]);
    let mut brain = patient_brain(&scratch, SCRIPT);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(ids(&shell.scenes().await), three);
        let (run, lock) = hold_first_step(&mut shell, &scratch).await;
        shell.stop(run).await;
        while shell.state().await != (run, Phase::Interrupted) {}
        lock.execute_batch("ROLLBACK").expect("unlocked");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    // A later scene, drawn normally.
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(
            ids(&shell.scenes().await),
            three,
            "the stopped scene isn't listed"
        );
        shell.submit("again").await;
        loop {
            match shell.data().await {
                DataToShell::StepCommit { scene, .. } => break scene,
                DataToShell::SceneSnapshot { .. } | DataToShell::AssistantText { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let (ended, later) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(
        outcomes(&scratch)[3..],
        [
            outcome("interrupted", "stop"),
            outcome("interrupted", "shell_lost")
        ]
    );
    let mut restarted = test_support::brain(&scratch, "");
    let expected = [three[0], three[1], three[2], later];
    assert_eq!(
        ids(&scenes_on_reconnect(&mut restarted).await),
        expected,
        "no label returns, and the next Scenes names the set the brain held"
    );
    restarted.close().await;
}

#[tokio::test]
async fn a_failed_clear_is_retried_at_the_next_barrier_and_stays_cleared() {
    let scratch = Scratch::new("clear-retry");
    let drawn = drawn(&scratch, &[&["a"]]);
    let sql = rusqlite::Connection::open(scratch.db()).expect("open");
    sql.execute_batch(
        "CREATE TRIGGER full BEFORE INSERT ON events WHEN NEW.kind = 'scene_cleared' BEGIN
             SELECT RAISE(ABORT, 'full');
         END;",
    )
    .expect("trapped");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(ids(&shell.scenes().await), drawn);
        shell.send(&clear(&drawn)).await;
        settle(&mut shell).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let failed = async {
        loop {
            let input = brain.inputs.recv().await.expect("open");
            let failed = matches!(input, Input::ScenesCleared { ok: false, .. });
            brain.offline(input);
            if failed {
                return;
            }
        }
    };
    timeout(PATIENCE, failed)
        .await
        .expect("the clear's write failed");
    assert!(
        !brain.coordinator.settled(),
        "the failed clear waits for a retry"
    );
    sql.execute_batch("DROP TRIGGER full").expect("untrapped");
    assert_eq!(scenes_on_reconnect(&mut brain).await, []);
    brain.close().await;
    assert_eq!(recovered(&scratch), [], "the scene doesn't return");
}

#[tokio::test]
async fn a_quit_applies_a_queued_stop_and_clear_and_waits_for_a_late_first_ack() {
    let scratch = Scratch::new("quit-queued");
    let drawn = drawn(&scratch, &[&["a"]]);
    let mut brain = patient_brain(&scratch, SCRIPT);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(ids(&shell.scenes().await), drawn);
        let (run, lock) = hold_first_step(&mut shell, &scratch).await;
        shell
            .send(&ControlToBrain::Stop {
                run,
                revealed: Vec::new(),
            })
            .await;
        shell.send(&clear(&drawn)).await;
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
        lock
    };
    let (ended, lock) = tokio::join!(brain.serve(control, data, &mut tap), client);
    let deadline = deadline(ended);
    assert!(
        brain.coordinator.visible().is_empty(),
        "the Clear was applied"
    );
    let released = release_after(lock, Duration::from_millis(300));
    assert_eq!(brain.quit(deadline).await, Quit::Complete);
    released.join().expect("released");
    assert_eq!(
        outcomes(&scratch).last(),
        Some(&outcome("interrupted", "stop")),
        "the Stop was applied"
    );
    assert_eq!(recovered(&scratch), []);
}

#[tokio::test]
async fn a_quit_ends_a_current_run_whose_first_ack_is_pending() {
    let scratch = Scratch::new("quit-current");
    let drawn = drawn(&scratch, &[&["a"]]);
    let mut brain = patient_brain(&scratch, SCRIPT);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(ids(&shell.scenes().await), drawn);
        let (_, lock) = hold_first_step(&mut shell, &scratch).await;
        shell.send(&clear(&drawn)).await;
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
        lock
    };
    let (ended, lock) = tokio::join!(brain.serve(control, data, &mut tap), client);
    let deadline = deadline(ended);
    let released = release_after(lock, Duration::from_millis(300));
    assert_eq!(brain.quit(deadline).await, Quit::Complete);
    released.join().expect("released");
    assert_eq!(
        outcomes(&scratch).last(),
        Some(&outcome("interrupted", "shell_lost"))
    );
    assert_eq!(
        recovered(&scratch),
        [],
        "its scene was cleared with the visible set"
    );
}

#[tokio::test]
async fn quitting_with_four_scenes_visible_after_an_eviction_brings_none_back() {
    let scratch = Scratch::new("quit-evicted");
    let all = drawn(&scratch, &[&["a"], &["b"], &["c"], &["d"], &["e"]]);
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(ids(&shell.scenes().await), all[1..]);
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(brain.quit(deadline(ended)).await, Quit::Complete);
    assert_eq!(recovered(&scratch), [], "A doesn't come back either");
}

#[tokio::test]
async fn a_quitting_written_before_both_pipes_close_is_accepted() {
    let scratch = Scratch::new("quit-closed");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    shell.send(&ControlToBrain::Quitting).await;
    drop(shell);
    let ended = brain.serve(control, data, &mut tap).await;
    assert_eq!(brain.quit(deadline(ended)).await, Quit::Complete);
}

#[tokio::test]
async fn a_quitting_split_around_the_data_pipe_s_end_is_finished_and_accepted() {
    let scratch = Scratch::new("quit-split");
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), shell) = duplex();
    let mut tap = Tap::from_env();
    let Shell {
        control_in,
        mut control_out,
        data_in,
        data_out,
        ..
    } = shell;
    let quitting = frame::encode(&ControlToBrain::Quitting, Pipe::Control).expect("encodes");
    let cut = quitting.len() - 1;
    let client = async {
        control_out.write_all(&quitting[..cut]).await.expect("sent");
        drop((data_in, data_out));
        tokio::time::sleep(Duration::from_millis(50)).await;
        control_out.write_all(&quitting[cut..]).await.expect("sent");
        drop((control_in, control_out));
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(brain.quit(deadline(ended)).await, Quit::Complete);
}

#[tokio::test]
async fn controls_queued_ahead_of_a_quitting_drain_in_order_and_the_quit_is_accepted() {
    let scratch = Scratch::new("quit-drain");
    let drawn = drawn(&scratch, &[&["a"]]);
    let mut brain = test_support::brain(&scratch, "");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    // More than the reader's channel holds, all in the pipe before the brain reads any.
    for _ in 0..100 {
        shell.stop(RunId(999)).await;
    }
    shell.send(&clear(&drawn)).await;
    shell.send(&ControlToBrain::Quitting).await;
    drop(shell);
    let ended = brain.serve(control, data, &mut tap).await;
    let deadline = deadline(ended);
    assert!(
        brain.coordinator.visible().is_empty(),
        "the Clear behind them was applied"
    );
    assert_eq!(brain.quit(deadline).await, Quit::Complete);
    assert_eq!(recovered(&scratch), []);
}

#[tokio::test]
async fn a_store_stalled_past_the_deadline_makes_the_quit_incomplete_on_time() {
    let scratch = Scratch::new("quit-stalled");
    let drawn = drawn(&scratch, &[&["a"]]);
    let (mut brain, lock) = locked_brain(&scratch, "");
    let ended = quit_with(&mut brain, &[]).await;
    let deadline = deadline(ended);
    let accepted = deadline - CLOSE_WAIT;
    assert_eq!(brain.quit(deadline).await, Quit::Incomplete);
    let took = accepted.elapsed();
    assert!(took >= CLOSE_WAIT, "{took:?}");
    assert!(
        took < CLOSE_WAIT + Duration::from_millis(500),
        "{took:?}: no stage restarted its budget"
    );
    assert_eq!(
        recovered(&scratch),
        drawn,
        "the scenes return, as after a crash"
    );
    lock.execute_batch("ROLLBACK").expect("unlocked");
}

#[tokio::test]
async fn a_recovered_label_never_reaches_the_logs_or_the_tap() {
    let scratch = Scratch::new("sentinel");
    let drawn = drawn(&scratch, &[&[SENTINEL]]);
    let mut brain = test_support::brain(&scratch, "");
    let recording = scratch.db().with_file_name("tap.bin");
    let mut tap = Tap::to_file(File::create(&recording).expect("tap file"));
    let logs = Captured::default();
    let guard = logs.install();
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(shell.scenes().await, [named(drawn[0], 1)]);
        assert_eq!(
            shell.data().await,
            DataToShell::SceneSnapshot {
                scene: drawn[0],
                revision: 1,
                chunk: vec![Step {
                    label: SENTINEL.to_owned()
                }],
                last: true,
            },
            "the label went through"
        );
        shell
            .send(&clear(&[drawn[0], drawn[0], drawn[0], drawn[0], drawn[0]]))
            .await;
        assert_eq!(
            shell.control().await,
            None,
            "too many ids end the connection"
        );
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(
        ended,
        Ended::Broken("more scenes cleared than can be visible")
    );
    brain.close().await;
    drop(guard);
    drop(tap);
    let recorded = std::fs::read(&recording).expect("tap read");
    assert!(!recorded.is_empty(), "the tap recorded the control frames");
    let found = recorded
        .windows(SENTINEL.len())
        .any(|window| window == SENTINEL.as_bytes());
    assert!(!found, "the tap carries the label");
    assert!(!logs.text().contains(SENTINEL), "{}", logs.text());
}

/// The slice 2 row's scenario, over the real pipes: a drawing restored in several chunks while a
/// stale Stop and a Clear arrive.
#[cfg(windows)]
#[tokio::test]
async fn a_reconnect_restores_whole_scenes_in_chunks_while_a_stop_and_a_clear_arrive() {
    let scratch = Scratch::new("restore-chunks");
    let drawn = drawn(&scratch, &[&["a0", "a1", "a2"], &["b0", "b1"]]);
    let (a, b) = (drawn[0], drawn[1]);
    let mut brain = test_support::brain(&scratch, "");
    brain.chunk_target = 1;
    let ((control, data), mut shell) = pipes("restore-chunks").await;
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, [named(a, 3), named(b, 2)]);
        let mut chunks = vec![shell.data().await];
        shell.stop(RunId(1)).await;
        shell.send(&clear(&[a])).await;
        while !matches!(chunks.last(), Some(DataToShell::SceneSnapshot { scene, last: true, .. }) if *scene == b)
        {
            chunks.push(shell.data().await);
        }
        settle(&mut shell).await;
        drop(shell);
        chunks
    };
    let (ended, chunks) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let chunk = |scene, label: &str, last| DataToShell::SceneSnapshot {
        scene,
        revision: if scene == a { 3 } else { 2 },
        chunk: vec![Step {
            label: label.to_owned(),
        }],
        last,
    };
    let whole = [
        chunk(a, "a0", false),
        chunk(a, "a1", false),
        chunk(a, "a2", true),
        chunk(b, "b0", false),
        chunk(b, "b1", true),
    ];
    // A's chunks already queued when the Clear came still arrive, in order; the shell drops
    // them. B's arrive whole after them, one step a chunk.
    let a_sent = chunks.len() - 2;
    assert_eq!(chunks[..a_sent], whole[..a_sent]);
    assert_eq!(chunks[a_sent..], whole[3..]);
    assert_eq!(
        scenes_on_reconnect(&mut brain).await,
        [named(b, 2)],
        "A doesn't come back"
    );
    brain.close().await;
    assert_eq!(recovered(&scratch), [b]);
}

/// Rez measurement for the pull request, not a check: from a connection to four full scenes
/// installed. Run it with
/// `cargo test --release -p capsula-brain -- --ignored --nocapture measure_scene_recovery`.
#[cfg(windows)]
#[tokio::test]
#[ignore = "a measurement, run by hand on Rez"]
async fn measure_scene_recovery() {
    use capsula_protocol::message::{MAX_STEP_LABEL, MAX_STEPS, MAX_VISIBLE_SCENES};
    let label = "x".repeat(MAX_STEP_LABEL);
    let labels = vec![label.as_str(); MAX_STEPS];
    let mut samples = Vec::new();
    for round in 0..30 {
        let scratch = Scratch::new("measure-scenes");
        drawn(&scratch, &[&labels[..]; MAX_VISIBLE_SCENES]);
        let mut brain = test_support::brain(&scratch, "");
        let ((control, data), mut shell) = pipes(&format!("measure-scenes-{round}")).await;
        let mut tap = Tap::from_env();
        let client = async {
            let started = tokio::time::Instant::now();
            assert_eq!(shell.scenes().await.len(), MAX_VISIBLE_SCENES);
            let mut installed = 0;
            while installed < MAX_VISIBLE_SCENES {
                let DataToShell::SceneSnapshot { chunk, last, .. } = shell.data().await else {
                    panic!("snapshots only");
                };
                shell.credit(chunk.len() * (MAX_STEP_LABEL + 2) + 64).await;
                installed += usize::from(last);
            }
            samples.push(started.elapsed());
            drop(shell);
        };
        let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        brain.close().await;
    }
    samples.sort_unstable();
    eprintln!(
        "connect -> four full scenes installed: p50 {:?} p95 {:?} max {:?} (n={})",
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1],
        samples.len()
    );
}

/// Rejects a Stop's own outcome, so only its `failed(store)` fallback commits.
const FAIL_STOPS: &str =
    "CREATE TRIGGER no_stop BEFORE UPDATE ON runs WHEN NEW.state = 'interrupted' BEGIN
     SELECT RAISE(ABORT, 'full');
 END;";
/// Rejects every outcome, fallback included.
const FAIL_OUTCOMES: &str =
    "CREATE TRIGGER no_outcome BEFORE UPDATE ON runs WHEN NEW.state <> 'active' BEGIN
     SELECT RAISE(ABORT, 'full');
 END;";
/// Rejects every clear.
const FAIL_CLEARS: &str =
    "CREATE TRIGGER full BEFORE INSERT ON events WHEN NEW.kind = 'scene_cleared' BEGIN
     SELECT RAISE(ABORT, 'full');
 END;";

fn sql(scratch: &Scratch, batch: &str) {
    let sql = rusqlite::Connection::open(scratch.db()).expect("open");
    sql.execute_batch(batch).expect("applied");
}

/// Reads data up to the `count`th step; the scene they drew.
async fn steps<S: AsyncRead + AsyncWrite>(shell: &mut Shell<S>, count: usize) -> SceneId {
    let mut read = 0;
    loop {
        match shell.data().await {
            DataToShell::StepCommit { scene, .. } => {
                read += 1;
                if read == count {
                    return scene;
                }
            }
            DataToShell::AssistantText { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The scene's kept steps as the next start rebuilds them, with its revision.
fn kept_after_restart(scratch: &Scratch, scene: SceneId) -> Option<(u32, Vec<String>)> {
    let store = Store::open(&scratch.db()).expect("reopen");
    let scenes = store.scenes().expect("scenes");
    scenes
        .into_iter()
        .find(|state| state.scene == scene)
        .map(|state| {
            let labels = state.kept.into_iter().map(|step| step.label).collect();
            (state.revision, labels)
        })
}

#[tokio::test]
async fn a_stop_whose_outcome_failed_still_keeps_only_what_stayed() {
    let scratch = Scratch::new("failed-stop");
    let mut brain = test_support::brain(&scratch, "0 step a\n0 step b\n0 step c\n5000 text never");
    sql(&scratch, FAIL_STOPS);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        let scene = steps(&mut shell, 3).await;
        shell
            .send(&ControlToBrain::Stop {
                run,
                revealed: vec![0],
            })
            .await;
        while shell.state().await != (run, Phase::Failed) {}
        drop(shell);
        scene
    };
    let (ended, scene) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(outcomes(&scratch), [outcome("failed", "store")]);
    assert_eq!(
        kept_after_restart(&scratch, scene),
        Some((3, vec!["a".to_owned()]))
    );
}

/// A Stop that kept nothing, whose outcome and its fallback both failed, with clears failing
/// too; returns once the run has left the coordinator.
async fn stopped_with_both_outcomes_failed(scratch: &Scratch) -> (Brain, SceneId) {
    let mut brain = test_support::brain(scratch, "0 step a\n5000 text never");
    sql(scratch, FAIL_OUTCOMES);
    sql(scratch, FAIL_CLEARS);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        let scene = steps(&mut shell, 1).await;
        shell.stop(run).await;
        while shell.state().await != (run, Phase::Failed) {}
        drop(shell);
        scene
    };
    let (ended, scene) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let gone = async {
        while brain.coordinator.ending_runs() > 0 {
            let input = brain.inputs.recv().await.expect("open");
            brain.offline(input);
        }
    };
    timeout(PATIENCE, gone).await.expect("both outcomes failed");
    (brain, scene)
}

#[tokio::test]
async fn a_clear_two_failed_outcomes_left_keeps_a_quit_incomplete_while_it_fails() {
    let scratch = Scratch::new("owed-clear-failing");
    let (brain, scene) = stopped_with_both_outcomes_failed(&scratch).await;
    assert!(!brain.coordinator.settled(), "the clear is still owed");
    let deadline = tokio::time::Instant::now() + CLOSE_WAIT;
    assert_eq!(brain.quit(deadline).await, Quit::Incomplete);
    sql(&scratch, "DROP TRIGGER no_outcome; DROP TRIGGER full;");
    assert_eq!(recovered(&scratch), [scene], "it returns, as after a crash");
}

#[tokio::test]
async fn a_clear_two_failed_outcomes_left_commits_once_the_store_takes_it() {
    let scratch = Scratch::new("owed-clear-commits");
    let (mut brain, _) = stopped_with_both_outcomes_failed(&scratch).await;
    sql(&scratch, "DROP TRIGGER full;");
    assert_eq!(scenes_on_reconnect(&mut brain).await, []);
    brain.close().await;
    sql(&scratch, "DROP TRIGGER no_outcome;");
    assert_eq!(recovered(&scratch), [], "the scene doesn't return");
}

/// Finishes a run of three steps, then stops it with `revealed` while its steps are still
/// unread, as Esc does while a shell drains a finished run. Returns its scene and the committed
/// count the next connection names for it.
async fn stopped_after_done(scratch: &Scratch, revealed: Vec<u32>) -> (SceneId, Option<u32>) {
    let mut brain = test_support::brain(scratch, "0 step a\n0 step b\n0 step c");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        while shell.state().await != (run, Phase::Done) {}
        shell.send(&ControlToBrain::Stop { run, revealed }).await;
        let scene = steps(&mut shell, 3).await;
        settle(&mut shell).await;
        drop(shell);
        scene
    };
    let (ended, scene) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    let named = scenes_on_reconnect(&mut brain).await;
    brain.close().await;
    let committed = named
        .iter()
        .find(|named| named.scene == scene)
        .map(|named| named.committed);
    (scene, committed)
}

#[tokio::test]
async fn a_stop_while_a_finished_runs_steps_drain_keeps_what_stayed() {
    let scratch = Scratch::new("late-stop-kept");
    let (scene, committed) = stopped_after_done(&scratch, vec![0]).await;
    assert_eq!(committed, Some(1));
    assert_eq!(
        kept_after_restart(&scratch, scene),
        Some((3, vec!["a".to_owned()]))
    );
}

#[tokio::test]
async fn a_stop_that_kept_nothing_while_a_finished_runs_steps_drain_clears_its_scene() {
    let scratch = Scratch::new("late-stop-none");
    let (scene, committed) = stopped_after_done(&scratch, Vec::new()).await;
    assert_eq!(committed, None);
    assert_eq!(kept_after_restart(&scratch, scene), None);
}
