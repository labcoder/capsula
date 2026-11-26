//! The send lease across the brain: a quit keeps pumping a granted send's settlement and the
//! delete it held, and a held delete never reaches the store when the brain stops first.

use kumochi_protocol::message::{ControlToBrain, ThreadId};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use super::deletes::listed;
use super::{PATIENCE, duplex, patient_brain_with};
use crate::coordinator::StoreOp;
use crate::engines::{Answer, Engine, Round, RunRequest, SendGate, TurnEvent};
use crate::runtime::{Brain, Ended, Quit, Seen};
use crate::tap::Tap;
use crate::test_support::Scratch;

/// The first task: a fresh store's thread 1 is the main thread.
const T: ThreadId = ThreadId(2);

/// A transport that asks before sending and keeps its lease until the test releases it, even
/// once its round is dropped, as a send whose last bytes are still going out would.
#[derive(Debug)]
struct Holds {
    granted: mpsc::UnboundedSender<()>,
    release: watch::Receiver<bool>,
}

impl Engine for Holds {
    fn start(
        &self,
        _request: RunRequest,
        events: mpsc::Sender<TurnEvent>,
        gate: SendGate,
    ) -> Round {
        let (granted, mut release) = (self.granted.clone(), self.release.clone());
        Box::pin(async move {
            let _open = events;
            if let Answer::Grant(lease) = gate.ask().await {
                granted.send(()).ok();
                tokio::spawn(async move {
                    release.wait_for(|released| *released).await.ok();
                    drop(lease);
                });
            }
            std::future::pending::<()>().await;
        })
    }
}

/// A brain whose sends hold their leases until the sender says so; the receiver hears each grant.
fn holding(scratch: &Scratch) -> (Brain, mpsc::UnboundedReceiver<()>, watch::Sender<bool>) {
    let (granted, grants) = mpsc::unbounded_channel();
    let (release, released) = watch::channel(false);
    let brain = patient_brain_with(
        scratch,
        Box::new(Holds {
            granted,
            release: released,
        }),
    );
    (brain, grants, release)
}

fn delete() -> ControlToBrain {
    ControlToBrain::DeleteThread {
        thread: T,
        after_turns: 1,
        resend: false,
    }
}

fn is_delete(seen: &Seen) -> bool {
    matches!(seen, Seen::StoreOpQueued(StoreOp::Delete { .. }))
}

#[tokio::test]
async fn quit_with_a_held_lease_pumps_settlement_and_held_deletes() {
    let scratch = Scratch::new("quit-lease");
    let (mut brain, mut grants, release) = holding(&scratch);
    let mut observed = brain.observe();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.scenes().await;
        shell.submit("go").await;
        timeout(PATIENCE, grants.recv())
            .await
            .expect("granted in time");
        shell.send(&delete()).await;
        shell.send(&ControlToBrain::Quitting).await;
        drop(shell);
    };
    let served = async { tokio::join!(brain.serve(control, data, &mut tap), client).0 };
    let ended = timeout(PATIENCE, served).await.expect("in time");
    let Ended::Quit { deadline } = ended else {
        panic!("expected a quit, got {ended:?}");
    };
    let settle = async {
        let draining = async {
            while let Some(seen) = observed.recv().await {
                assert!(
                    !is_delete(&seen),
                    "the delete went out while its lease was held"
                );
                if seen == Seen::QuitDraining {
                    return;
                }
            }
        };
        timeout(PATIENCE, draining).await.expect("draining in time");
        release.send(true).expect("the holder waits");
    };
    let (quit, ()) = tokio::join!(brain.quit(deadline), settle);
    assert_eq!(quit.quit, Some(Quit::Complete));
    let mut queued = 0;
    while let Ok(seen) = observed.try_recv() {
        queued += usize::from(is_delete(&seen));
    }
    assert_eq!(
        queued, 1,
        "the held delete went out once, after the settlement"
    );
    assert_eq!(listed(&scratch), 0);
}

#[tokio::test]
async fn a_held_delete_is_not_committed_or_reported_after_restart() {
    let scratch = Scratch::new("restart-lease");
    let (mut brain, mut grants, _held) = holding(&scratch);
    let mut observed = brain.observe();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.scenes().await;
        shell.submit("go").await;
        timeout(PATIENCE, grants.recv())
            .await
            .expect("granted in time");
        while observed.try_recv().is_ok() {}
        shell.send(&delete()).await;
        let applied = async { while observed.recv().await != Some(Seen::ControlApplied) {} };
        timeout(PATIENCE, applied).await.expect("applied in time");
        drop(shell);
    };
    let served = async { tokio::join!(brain.serve(control, data, &mut tap), client).0 };
    assert_eq!(
        timeout(PATIENCE, served).await.expect("in time"),
        Ended::Lost
    );
    brain.close().await;
    while let Ok(seen) = observed.try_recv() {
        assert!(!is_delete(&seen), "the held delete reached the store");
    }
    assert_eq!(
        listed(&scratch),
        1,
        "the task comes back, and no success was reported"
    );
}
