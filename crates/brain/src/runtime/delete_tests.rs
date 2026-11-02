//! Deletes across a connection's end and the next: the drain, both orders of a resend and its
//! answer, and an answer the shell never read.

use capsula_protocol::frame::{self, Pipe};
use capsula_protocol::message::{ControlToBrain, ControlToShell, Deleted, Phase, RunId, ThreadId};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

use super::{PATIENCE, Shell, duplex, patient_brain};
use crate::coordinator::StoreOp;
use crate::runtime::{Brain, Ended, Seen};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::Scratch;

const T: ThreadId = ThreadId(1);

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
fn listed(scratch: &Scratch) -> usize {
    let store = Store::open(&scratch.db()).expect("reopen");
    store
        .thread_page(None, 50)
        .map(|(rows, _)| rows.len())
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

#[tokio::test]
async fn an_answer_before_the_resend_consumes_it_and_a_lost_answer_is_asked_again() {
    let scratch = Scratch::new("answer-first");
    let mut brain = patient_brain(&scratch, "0 text a\n0 text b\n0 text c");
    let mut observed = brain.observe();
    let (run, release) = delete_held_past_the_connection(&mut brain, &mut observed).await;
    connection!(brain, |shell| async {
        shell.scenes().await;
        release.send(()).expect("the delete waits at the gate");
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::ThreadDeleted {
                thread: T,
                outcome: Deleted::Removed,
                last_run: Some(run),
            })
        );
        shell.send(&resend()).await;
        seen(&mut observed, |seen| *seen == Seen::ControlApplied).await;
        // A second answer would come before this turn's state.
        shell.submit("next").await;
        let (next, _) = timeout(PATIENCE, shell.state()).await.expect("in time");
        assert!(next > run);
    });
    let asked = std::iter::from_fn(|| observed.try_recv().ok())
        .filter(|seen| matches!(seen, Seen::StoreOpQueued(op) if is_delete(op)));
    assert_eq!(
        asked.count(),
        0,
        "the crossed resend asked nothing of the store"
    );

    // That answer never reached the shell, so the next connection asks again.
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
    assert_eq!(listed(&scratch), 1, "only the next turn's thread");
}
