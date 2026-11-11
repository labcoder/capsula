//! Deletes across a connection's end and the next: the drain, both orders of a resend and its
//! answer, an answer the shell never read, and a hide that never committed.

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, Deleted, Phase, RunId, ThreadId, ThreadKind,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

use super::{PATIENCE, Shell, duplex, outcome, outcomes, patient_brain};
use crate::coordinator::StoreOp;
use crate::runtime::{Brain, Ended, Quit, Seen};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::Scratch;

/// The first task: a fresh store's thread 1 is the main thread.
const T: ThreadId = ThreadId(2);

fn delete(after_turns: u32) -> ControlToBrain {
    ControlToBrain::DeleteThread {
        thread: T,
        after_turns,
        resend: false,
    }
}

/// A later connection's resend, with no turn sent before it.
fn resend() -> ControlToBrain {
    ControlToBrain::DeleteThread {
        thread: T,
        after_turns: 0,
        resend: true,
    }
}

fn is_delete(op: &StoreOp) -> bool {
    matches!(op, StoreOp::Delete { .. })
}

async fn seen(observed: &mut UnboundedReceiver<Seen>, wanted: impl Fn(&Seen) -> bool) {
    let found = async { while !wanted(&observed.recv().await.expect("the brain reports")) {} };
    timeout(PATIENCE, found).await.expect("seen in time");
}

/// Runs one turn in a new thread to Done; its run.
async fn one_run<S: tokio::io::AsyncRead + tokio::io::AsyncWrite>(shell: &mut Shell<S>) -> RunId {
    assert_eq!(shell.scenes().await, []);
    shell.submit("go").await;
    timeout(PATIENCE, async {
        let (run, _) = shell.state().await;
        while shell.state().await != (run, Phase::Done) {}
        run
    })
    .await
    .expect("the run is done in time")
}

/// The threads a restarted store lists.
pub(super) fn listed(scratch: &Scratch) -> usize {
    live_threads(scratch).len()
}

fn live_threads(scratch: &Scratch) -> Vec<ThreadId> {
    let store = Store::open(&scratch.db()).expect("reopen");
    store
        .pages()
        .thread_page(None, 50)
        .map(|(rows, _)| {
            rows.into_iter()
                .filter(|row| row.kind == ThreadKind::Thread)
                .map(|row| row.id)
                .collect()
        })
        .expect("threads")
}

/// Serves a connection on which `client` runs and then drops the shell, and expects it to end
/// as lost, all within twice [`PATIENCE`].
macro_rules! connection {
    ($brain:expr, |$shell:ident| $client:expr) => {{
        let ((control, data), mut $shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            let out = $client.await;
            drop($shell);
            out
        };
        let served = async { tokio::join!($brain.serve(control, data, &mut tap), client) };
        let (ended, out) = timeout(PATIENCE * 2, served)
            .await
            .expect("the connection ends in time");
        assert_eq!(ended, Ended::Lost);
        out
    }};
}

#[tokio::test]
async fn a_delete_read_with_the_control_pipe_s_end_is_still_applied() {
    for data_first in [false, true] {
        let scratch = Scratch::new("eof-delete");
        let mut brain = patient_brain(&scratch, "0 text a");
        let mut observed = brain.observe();
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            one_run(&mut shell).await;
            let Shell {
                control_in,
                mut control_out,
                data_in,
                data_out,
                ..
            } = shell;
            let data = (data_in, data_out);
            if data_first {
                drop(data);
                seen(&mut observed, |seen| *seen == Seen::Grace).await;
            }
            // The frame and the pipe's end are written without a yield between, so the reader
            // forwards the delete and sees the end in one poll, before the core can take it.
            let last = frame::encode(&delete(1), Pipe::Control).expect("encodes");
            control_out.write_all(&last).await.expect("sent");
            control_out.shutdown().await.expect("ended");
            drop(control_in);
        };
        let served = async { tokio::join!(brain.serve(control, data, &mut tap), client) };
        let (ended, ()) = timeout(PATIENCE * 2, served)
            .await
            .expect("the connection ends in time");
        assert_eq!(ended, Ended::Lost);
        if !data_first {
            seen(&mut observed, |seen| *seen == Seen::ControlDrained).await;
        }
        seen(
            &mut observed,
            |seen| matches!(seen, Seen::StoreOpQueued(op) if is_delete(op)),
        )
        .await;
        brain.close().await;
        assert_eq!(listed(&scratch), 0, "data first: {data_first}");
    }
}

/// Sends a delete whose store op the gate holds, then loses the shell; the run and the gate.
async fn delete_held_past_the_connection(
    brain: &mut Brain,
    observed: &mut UnboundedReceiver<Seen>,
) -> (RunId, std::sync::mpsc::Sender<()>) {
    let release = brain.hold_store(is_delete);
    let run = connection!(brain, |shell| async {
        let run = one_run(&mut shell).await;
        shell.send(&delete(1)).await;
        seen(
            observed,
            |seen| matches!(seen, Seen::StoreOpStarted(op) if is_delete(op)),
        )
        .await;
        run
    });
    (run, release)
}

#[tokio::test]
async fn a_resend_before_the_answer_gets_the_answer_on_its_connection() {
    let scratch = Scratch::new("resend-first");
    let mut brain = patient_brain(&scratch, "0 text a");
    let mut observed = brain.observe();
    let (run, release) = delete_held_past_the_connection(&mut brain, &mut observed).await;
    connection!(brain, |shell| async {
        shell.scenes().await;
        shell.send(&resend()).await;
        seen(&mut observed, |seen| *seen == Seen::ControlApplied).await;
        release.send(()).expect("the delete waits at the gate");
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::ThreadDeleted {
                thread: T,
                outcome: Deleted::Removed,
                last_run: Some(run),
            })
        );
    });
    brain.close().await;
    let ops = std::iter::from_fn(|| observed.try_recv().ok())
        .filter(|seen| matches!(seen, Seen::StoreOpQueued(op) if is_delete(op)));
    assert_eq!(ops.count(), 0, "the resend rebinds the delete in flight");
    assert_eq!(listed(&scratch), 0);
}

const NO_HIDE: &str = "CREATE TRIGGER no_hide BEFORE UPDATE OF state ON threads BEGIN SELECT RAISE(ABORT, 'kept'); END;";

fn trap(scratch: &Scratch, sql: &str) {
    rusqlite::Connection::open(scratch.db())
        .expect("open")
        .execute_batch(sql)
        .expect("trap");
}

/// The store answers a delete held past its connection with `outcome` before the next
/// connection's resend arrives: the resend is consumed, and a hide that failed once isn't tried
/// again, so the thread stays as the shell was told.
async fn answer_before_the_resend(outcome: Deleted) {
    let scratch = Scratch::new("answer-first");
    let mut brain = patient_brain(&scratch, "0 text a\n0 text b");
    let mut observed = brain.observe();
    if outcome == Deleted::NotHidden {
        trap(&scratch, NO_HIDE);
    }
    let (run, release) = delete_held_past_the_connection(&mut brain, &mut observed).await;
    connection!(brain, |shell| async {
        shell.scenes().await;
        release.send(()).expect("the delete waits at the gate");
        let last_run = (outcome == Deleted::Removed).then_some(run);
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::ThreadDeleted {
                thread: T,
                outcome,
                last_run,
            })
        );
        // A second hide would now succeed.
        trap(&scratch, "DROP TRIGGER IF EXISTS no_hide;");
        shell.send(&resend()).await;
        seen(&mut observed, |seen| *seen == Seen::ControlApplied).await;
        // A second answer would come before this turn's state.
        shell.submit("next").await;
        let (next, _) = timeout(PATIENCE, shell.state()).await.expect("in time");
        assert_eq!(next, RunId(run.0 + 1));
    });
    let asked = std::iter::from_fn(|| observed.try_recv().ok())
        .filter(|seen| matches!(seen, Seen::StoreOpQueued(op) if is_delete(op)));
    assert_eq!(
        asked.count(),
        0,
        "the crossed resend asked nothing of the store"
    );
    brain.close().await;
    let kept = usize::from(outcome == Deleted::NotHidden);
    assert_eq!(
        listed(&scratch),
        1 + kept,
        "the next turn's thread, and T if kept"
    );
}

#[tokio::test]
async fn an_answer_before_the_resend_consumes_it() {
    Box::pin(answer_before_the_resend(Deleted::Removed)).await;
}

#[tokio::test]
async fn a_crossed_answer_that_hid_nothing_consumes_the_resend() {
    Box::pin(answer_before_the_resend(Deleted::NotHidden)).await;
}

#[tokio::test]
async fn an_answer_the_shell_never_read_is_asked_again() {
    let scratch = Scratch::new("lost-answer");
    let mut brain = patient_brain(&scratch, "0 text a");
    let mut observed = brain.observe();
    let (_, release) = delete_held_past_the_connection(&mut brain, &mut observed).await;
    connection!(brain, |shell| async {
        shell.scenes().await;
        release.send(()).expect("the delete waits at the gate");
        // The answer goes out on this connection, and the shell ends without reading it.
        seen(&mut observed, |seen| *seen == Seen::DeleteAnswered).await;
    });
    connection!(brain, |shell| async {
        shell.scenes().await;
        shell.send(&resend()).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::ThreadDeleted {
                thread: T,
                outcome: Deleted::Removed,
                last_run: None,
            }),
            "already gone"
        );
    });
    brain.close().await;
    let asked = std::iter::from_fn(|| observed.try_recv().ok())
        .filter(|seen| matches!(seen, Seen::StoreOpQueued(op) if is_delete(op)));
    assert_eq!(asked.count(), 1, "the resend asked the store again");
    assert_eq!(listed(&scratch), 0);
}

#[tokio::test]
async fn a_hide_that_never_committed_leaves_the_thread_for_the_next_start() {
    let scratch = Scratch::new("hide-uncommitted");
    let mut brain = patient_brain(&scratch, SLOW);
    // A hide rolled back leaves the store as a crash before its commit would.
    trap(&scratch, NO_HIDE);
    connection!(brain, |shell| async {
        assert_eq!(shell.scenes().await, []);
        shell.submit("t").await;
        assert_eq!(
            shell.control().await,
            Some(run_state(1, T, Phase::Thinking))
        );
        shell.send(&delete(1)).await;
        let interrupted = run_state(1, T, Phase::Interrupted);
        while shell.control().await.expect("open") != interrupted {}
        assert_eq!(
            shell.control().await.expect("open"),
            ControlToShell::ThreadDeleted {
                thread: T,
                outcome: Deleted::NotHidden,
                last_run: None,
            }
        );
    });
    brain.close().await;
    trap(&scratch, "DROP TRIGGER no_hide;");
    assert_eq!(outcomes(&scratch), [outcome("interrupted", "deleted")]);
    assert_eq!(listed(&scratch), 1);

    let mut brain = patient_brain(&scratch, "");
    assert_eq!(
        outcomes(&scratch),
        [outcome("interrupted", "deleted")],
        "recovery leaves the ended run as it was"
    );
    connection!(brain, |shell| async {
        assert_eq!(shell.scenes().await, []);
        shell.send(&resend()).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::ThreadDeleted {
                thread: T,
                outcome: Deleted::Removed,
                last_run: Some(RunId(1)),
            })
        );
    });
    brain.close().await;
    assert_eq!(listed(&scratch), 0);
}

/// Serves a connection whose shell sends a delete for T and then quits; the quit's outcome.
/// Boxed, because the future holds the whole brain.
fn delete_then_quit(mut brain: Brain) -> std::pin::Pin<Box<impl Future<Output = Quit>>> {
    Box::pin(async move {
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            shell.scenes().await;
            shell.send(&delete(0)).await;
            shell.send(&ControlToBrain::Quitting).await;
            drop(shell);
        };
        let served = async { tokio::join!(brain.serve(control, data, &mut tap), client).0 };
        let ended = timeout(PATIENCE, served).await.expect("in time");
        let Ended::Quit { deadline } = ended else {
            panic!("expected a quit, got {ended:?}");
        };
        brain.quit(deadline).await
    })
}

/// A brain whose store already holds T, from one finished run.
async fn with_a_thread(scratch: &Scratch) -> Brain {
    let mut brain = patient_brain(scratch, "0 text a");
    connection!(brain, |shell| async {
        one_run(&mut shell).await;
    });
    brain
}

#[tokio::test]
async fn a_quit_waits_for_an_accepted_delete() {
    let scratch = Scratch::new("quit-delete");
    let brain = with_a_thread(&scratch).await;
    assert_eq!(delete_then_quit(brain).await, Quit::Complete);
    assert_eq!(listed(&scratch), 0);
}

#[tokio::test]
async fn a_delete_that_hid_nothing_makes_the_quit_incomplete() {
    let scratch = Scratch::new("quit-undeleted");
    let brain = with_a_thread(&scratch).await;
    rusqlite::Connection::open(scratch.db())
        .expect("open")
        .execute_batch(
            "CREATE TRIGGER no_hide BEFORE UPDATE OF state ON threads BEGIN
                 SELECT RAISE(ABORT, 'kept');
             END;",
        )
        .expect("trap");
    assert_eq!(delete_then_quit(brain).await, Quit::Incomplete);
    assert_eq!(listed(&scratch), 1, "the thread comes back");
}

#[tokio::test]
async fn a_delete_refused_for_capacity_makes_the_quit_incomplete() {
    let scratch = Scratch::new("quit-refused");
    let mut brain = with_a_thread(&scratch).await;
    let release = brain.hold_store(is_delete);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.scenes().await;
        for absent in 11..19 {
            shell
                .send(&ControlToBrain::DeleteThread {
                    thread: ThreadId(absent),
                    after_turns: 0,
                    resend: false,
                })
                .await;
        }
        shell.send(&delete(0)).await;
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
    };
    let served = async { tokio::join!(brain.serve(control, data, &mut tap), client).0 };
    let ended = timeout(PATIENCE, served).await.expect("in time");
    let Ended::Quit { deadline } = ended else {
        panic!("expected a quit, got {ended:?}");
    };
    release
        .send(())
        .expect("the first delete waits at the gate");
    assert_eq!(brain.quit(deadline).await, Quit::Incomplete);
    assert_eq!(listed(&scratch), 1, "the ninth delete, T's, didn't happen");
}

/// When a turn naming T, sent before T's delete, is read relative to the delete's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    /// While the store holds the delete.
    Pending,
    /// After the answer, with the store as it is.
    Removed,
    /// After the answer, when removing the rows failed.
    Hidden,
    /// After the answer, when hiding the thread failed.
    NotHidden,
}

const SLOW: &str = "0 text a\n5000 text never";
const U: ThreadId = ThreadId(3);

/// The next control frame, skipping run 1's states and run 2's until it's interrupted.
async fn next_control<S: tokio::io::AsyncRead + tokio::io::AsyncWrite>(
    shell: &mut Shell<S>,
) -> ControlToShell {
    loop {
        match shell.control().await.expect("open") {
            ControlToShell::RunState { run, phase, .. }
                if run == RunId(1) || (run == RunId(2) && phase != Phase::Interrupted) => {}
            other => return other,
        }
    }
}

fn turn_in(thread: ThreadId) -> Vec<u8> {
    let turn = kumochi_protocol::message::DataToBrain::TextSubmit {
        turn: 1,
        screen: kumochi_protocol::message::ScreenChoice::None,
        thread: Some(thread),
        text: "more".into(),
    };
    frame::encode(&turn, Pipe::Data).expect("encodes")
}

async fn submit_to<S: tokio::io::AsyncRead + tokio::io::AsyncWrite>(
    shell: &mut Shell<S>,
    thread: ThreadId,
) {
    shell
        .data_out
        .write_all(&turn_in(thread))
        .await
        .expect("sent");
}

fn run_state(run: u64, thread: ThreadId, phase: Phase) -> ControlToShell {
    ControlToShell::RunState {
        origin: (phase == Phase::Thinking)
            .then_some(kumochi_protocol::message::RunOrigin::Typed { turn: 1 }),
        run: RunId(run),
        thread,
        phase,
        error: None,
    }
}

/// Runs a turn in T and stops it, then starts U's run, which stays current.
async fn stop_t_then_start_u<S: tokio::io::AsyncRead + tokio::io::AsyncWrite>(
    shell: &mut Shell<S>,
) {
    assert_eq!(shell.scenes().await, []);
    shell.submit("t").await;
    assert_eq!(
        shell.control().await,
        Some(run_state(1, T, Phase::Thinking))
    );
    shell.stop(RunId(1)).await;
    shell.submit("u").await;
    loop {
        if let ControlToShell::RunState {
            run: RunId(2),
            thread,
            ..
        } = shell.control().await.expect("open")
        {
            assert_eq!(thread, U);
            break;
        }
    }
}

/// T's run is stopped and U's is current when the shell sends a turn naming T and then T's
/// delete, counting that turn; the data reader is held mid-frame, so the brain reads the delete
/// first, and the turn at `read`.
async fn cross_pipe(read: Read) {
    let scratch = Scratch::new("cross-pipe");
    let mut brain = patient_brain(&scratch, SLOW);
    let mut observed = brain.observe();
    let trap = match read {
        Read::Hidden => Some(
            "CREATE TRIGGER keep BEFORE DELETE ON threads BEGIN SELECT RAISE(ABORT, 'kept'); END;",
        ),
        Read::NotHidden => Some(
            "CREATE TRIGGER no_hide BEFORE UPDATE OF state ON threads BEGIN SELECT RAISE(ABORT, 'kept'); END;",
        ),
        Read::Pending | Read::Removed => None,
    };
    if let Some(trap) = trap {
        rusqlite::Connection::open(scratch.db())
            .expect("open")
            .execute_batch(trap)
            .expect("trap");
    }
    let release = (read == Read::Pending).then(|| brain.hold_store(is_delete));
    connection!(brain, |shell| async {
        stop_t_then_start_u(&mut shell).await;
        let turn = turn_in(T);
        let (sent, held) = turn.split_at(turn.len() - 1);
        shell.data_out.write_all(sent).await.expect("sent");
        shell.send(&delete(3)).await;
        seen(
            &mut observed,
            |seen| matches!(seen, Seen::StoreOpQueued(op) if is_delete(op)),
        )
        .await;
        let outcome = match read {
            Read::Pending => None,
            Read::Removed => Some(Deleted::Removed),
            Read::Hidden => Some(Deleted::Hidden),
            Read::NotHidden => Some(Deleted::NotHidden),
        };
        if let Some(outcome) = outcome {
            let ControlToShell::ThreadDeleted { outcome: got, .. } = next_control(&mut shell).await
            else {
                panic!("expected the answer")
            };
            assert_eq!(got, outcome);
        }
        shell.data_out.write_all(held).await.expect("sent");
        if read == Read::NotHidden {
            assert_eq!(
                next_control(&mut shell).await,
                run_state(2, U, Phase::Interrupted),
                "the turn replaces U, as the user asked"
            );
            assert_eq!(
                next_control(&mut shell).await,
                run_state(3, T, Phase::Thinking)
            );
            return;
        }
        assert_eq!(
            next_control(&mut shell).await,
            ControlToShell::TurnRefused {
                turn: 1,
                thread: Some(T)
            }
        );
        if let Some(release) = &release {
            release.send(()).expect("the delete waits at the gate");
            assert!(matches!(
                next_control(&mut shell).await,
                ControlToShell::ThreadDeleted {
                    outcome: Deleted::Removed,
                    ..
                }
            ));
        }
        submit_to(&mut shell, U).await;
        assert_eq!(
            next_control(&mut shell).await,
            run_state(2, U, Phase::Interrupted),
            "U was still current"
        );
        assert_eq!(
            next_control(&mut shell).await,
            run_state(3, U, Phase::Thinking),
            "the fence retired at the refused turn, which took no run id"
        );
    });
    brain.close().await;
    let mut threads = live_threads(&scratch);
    if read == Read::NotHidden {
        threads.sort_unstable();
        assert_eq!(
            threads,
            [T, U],
            "both rows stay, whichever changed last: {read:?}"
        );
    } else {
        assert_eq!(threads, [U], "no row of T came back: {read:?}");
    }
}

#[tokio::test]
async fn a_turn_sent_before_a_pending_delete_is_refused() {
    Box::pin(cross_pipe(Read::Pending)).await;
}

#[tokio::test]
async fn a_turn_sent_before_a_removal_is_refused_by_the_fence() {
    Box::pin(cross_pipe(Read::Removed)).await;
}

#[tokio::test]
async fn a_turn_sent_before_a_hide_is_refused_by_the_fence() {
    Box::pin(cross_pipe(Read::Hidden)).await;
}

#[tokio::test]
async fn a_turn_sent_before_a_delete_that_hid_nothing_runs() {
    Box::pin(cross_pipe(Read::NotHidden)).await;
}
