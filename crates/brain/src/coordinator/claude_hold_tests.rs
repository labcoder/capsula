//! H-a to H-i: a Claude Code launch holds its seed's send lease until its job is confirmed
//! empty, through the real coordinator, `SendGate` and `SendLease`, on the lane's fake.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kumochi_protocol::message::{ControlToShell, Deleted, ThreadId};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{R, T, built, ends_failed, fresh};
use crate::coordinator::{BuildKey, Coordinator, Effect, Input, LeaseId, SendKey, StoreOp};
use crate::engines::claude_code::lane::Failure as LaneFailure;
use crate::engines::claude_code::lane::tests::{self as lane_tests, Base};
use crate::engines::claude_code::supervisor::tests::{
    WAIT, barrier, echoed, end, found, pipes, question, queue, version,
};
use crate::engines::claude_code::supervisor::{
    Launch, MAX_LAUNCH, Revoked, SETTLE, StdinRefused, Teardown, admit,
};
use crate::engines::{self, Answer, Asks, SendGate, SendLease};
use crate::store::Failure;

/// A task another task's build reads.
const B: ThreadId = ThreadId(2);

/// The coordinator with run 10 in task 4 granted a held send of `sources`, and that lease.
struct Granted {
    core: Coordinator,
    lease: Option<SendLease>,
    id: LeaseId,
    inputs: mpsc::UnboundedReceiver<Input>,
}

impl Granted {
    async fn new(sources: &[ThreadId]) -> Self {
        let mut core = fresh();
        core.handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale");
        core.handle(Input::TurnStored { run: R }).expect("current");
        core.handle(built(R, 1, sources)).expect("current");
        let key = SendKey::Build(BuildKey {
            run: R,
            round: 1,
            attempt: 1,
        });
        let (inputs_tx, mut inputs) = mpsc::unbounded_channel();
        let asks = Asks::default();
        let gate = SendGate::new(key, Arc::clone(&asks), inputs_tx.clone());
        let hold = MAX_LAUNCH + SETTLE;
        let asked = tokio::spawn(async move { gate.ask_held(hold).await });
        let ready = timeout(WAIT, inputs.recv())
            .await
            .expect("asked")
            .expect("open");
        assert_eq!(
            ready,
            Input::SendReady {
                key,
                hold: Some(Duration::from_secs(165)),
            }
        );
        let id = LeaseId { key, send: 1 };
        assert_eq!(
            core.handle(ready),
            Ok(vec![
                Effect::Grant(id),
                Effect::ArmSendDeadline {
                    lease: id,
                    after: Duration::from_secs(165),
                },
            ])
        );
        engines::answer(&asks, key, Answer::Grant(SendLease::new(id, inputs_tx)));
        let Answer::Grant(lease) = asked.await.expect("the ask ends") else {
            panic!("refused");
        };
        Self {
            core,
            lease: Some(lease),
            id,
            inputs,
        }
    }

    fn lease(&mut self) -> SendLease {
        self.lease.take().expect("granted once")
    }

    /// Hands `input` to the coordinator and keeps the store deletes it emits.
    fn deletes(&mut self, input: Input) -> Vec<ThreadId> {
        self.core
            .handle(input)
            .expect("current")
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Store(StoreOp::Delete { thread }) => Some(thread),
                _ => None,
            })
            .collect()
    }

    /// Whether the lease has settled by now, without waiting.
    fn settled(&mut self) -> bool {
        match self.inputs.try_recv() {
            Ok(input) => {
                assert_eq!(input, Input::SendSettled { lease: self.id });
                true
            }
            Err(_) => false,
        }
    }

    async fn settles(&mut self) -> Input {
        timeout(WAIT, self.inputs.recv())
            .await
            .expect("settles")
            .expect("open")
    }
}

fn delete(thread: ThreadId) -> Input {
    Input::Delete {
        thread,
        after_turns: 1,
        resend: false,
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// Starts `launch`'s teardown on the runtime, so the test keeps watching while it runs.
fn ending(launch: Launch) -> tokio::task::JoinHandle<Teardown> {
    tokio::spawn(end(launch))
}

/// H-a and H-b: a seed chunk admitted and paused before its write, then the launch revoked as a
/// Stop or the delete's `CancelEngine` does: the delete of `deleted` waits until the job is
/// empty, whichever source it is.
async fn a_paused_write_holds_the_delete_of(name: &str, deleted: ThreadId) {
    let base = Base::new(name);
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, write) = barrier();
    start.hooks.write = Some(paused);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut granted = Granted::new(&[T, B]).await;
    launch.hold_send(granted.lease()).expect("held");
    let mut writer = pipes.writer().expect("the writer");
    writer.seed(&[b's'; 10], soon()).await.expect("queued");
    assert!(write.reached_within(WAIT), "the seed's chunk was admitted");

    assert_eq!(granted.deletes(delete(deleted)), Vec::new(), "held");
    assert_eq!(launch.lease().revoke(), Revoked::WriteInFlight);
    let teardown = ending(launch);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !granted.settled(),
        "nothing settles while the write is paused"
    );

    write.release();
    assert_eq!(teardown.await.expect("ends"), Teardown::Done);
    let settled = granted.settles().await;
    assert_eq!(granted.deletes(settled), [deleted]);
}

/// H-a: the launch's own task.
#[tokio::test]
async fn a_delete_of_the_launchs_own_task_waits_for_the_paused_write() {
    a_paused_write_holds_the_delete_of("hold-own", T).await;
}

/// H-b: a task the build only read.
#[tokio::test]
async fn a_delete_of_a_referenced_task_waits_for_the_paused_write() {
    a_paused_write_holds_the_delete_of("hold-referenced", B).await;
}

/// H-c: revoked while live with nothing in flight: the delete still waits for the job to be
/// confirmed empty.
#[tokio::test]
async fn a_revoked_launch_settles_only_once_its_job_is_ended() {
    let base = Base::new("hold-revoked");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (held, termination) = barrier();
    start.hooks.terminate = Some(held);
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    let mut granted = Granted::new(&[T]).await;
    launch.hold_send(granted.lease()).expect("held");
    assert_eq!(granted.deletes(delete(T)), Vec::new(), "held");
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    let teardown = ending(launch);
    // Waited off this runtime, which serves the listener the teardown aborts first.
    let termination = tokio::task::spawn_blocking(move || {
        assert!(
            termination.reached_within(WAIT),
            "the teardown is at the job"
        );
        termination
    })
    .await
    .expect("the wait");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!granted.settled(), "not before the job is confirmed empty");
    termination.release();
    assert_eq!(teardown.await.expect("ends"), Teardown::Done);
    let settled = granted.settles().await;
    assert_eq!(granted.deletes(settled), [T]);
}

/// H-d and H-g: a write held past the pipe grace leaves the launch stuck. Its lease never
/// settles, not even once the lane is gone; its deadline fails the run, and B stays fenced.
#[tokio::test]
async fn a_stuck_launch_never_settles_and_its_deadline_fails_closed() {
    let base = Base::new("hold-stuck");
    let mut lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, write) = barrier();
    start.hooks.write = Some(paused);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut granted = Granted::new(&[T, B]).await;
    launch.hold_send(granted.lease()).expect("held");
    let mut writer = pipes.writer().expect("the writer");
    writer.seed(&[b's'; 10], soon()).await.expect("queued");
    assert!(write.reached_within(WAIT), "admitted");
    assert_eq!(granted.deletes(delete(B)), Vec::new(), "held");

    assert_eq!(end(launch).await, Teardown::Stuck);
    assert_eq!(version(&mut lane).await, Err(LaneFailure::Refused));
    assert!(!lane.close(lane_tests::soon()).await, "stuck");
    drop((lane, pipes, writer));
    write.release();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!granted.settled(), "a stuck lease never settles");

    let mut expected = ends_failed(Failure::Engine);
    expected.push(Effect::Control(ControlToShell::ThreadDeleted {
        thread: B,
        outcome: Deleted::NotHidden,
        last_run: None,
    }));
    assert_eq!(
        granted
            .core
            .handle(Input::SendDeadline { lease: granted.id }),
        Ok(expected)
    );
}

/// H-i: a grant that arrives after the launch was revoked is handed back unused; dropping it
/// settles at once, and nothing is written.
#[tokio::test]
async fn a_grant_after_revocation_is_handed_back_and_nothing_is_written() {
    let base = Base::new("hold-late-grant");
    let lane = found(&base, "echo").await;
    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut granted = Granted::new(&[T]).await;
    assert_eq!(granted.deletes(delete(T)), Vec::new(), "held");
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    let lease = launch.hold_send(granted.lease()).expect_err("handed back");
    drop(lease);
    let settled = granted.settles().await;
    assert_eq!(granted.deletes(settled), [T]);
    let mut writer = pipes.writer().expect("the writer");
    assert_eq!(
        writer.seed(&[b's'; 10], soon()).await,
        Err(StdinRefused::Revoked)
    );
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(echoed(&base), Vec::<usize>::new());
}
