//! C5b-1a on the lane's fake: the launch's handles from admission (§1), the stdin writer and its
//! byte caps (§3), and S4's Idle half (T15).

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use kumochi_protocol::message::RunId;
use tokio::time::timeout;

use super::tests::{WAIT, barrier, echoed, end, found, pipes, question, queue};
use super::{CHUNK, Input, MAX_CONTROL, MAX_SEED, Revoked, StdinRefused, Teardown, admit};
use crate::engines::claude_code::lane::tests::Base;

fn far() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

#[test]
fn the_caps_hold() {
    assert_eq!(MAX_SEED, 16_777_216);
    assert_eq!(MAX_CONTROL, 4096);
    assert_eq!(CHUNK, 65_536);
}

/// A seed of exactly the cap reaches the child whole, in full chunks, and uses up the one seed.
#[tokio::test]
async fn a_seed_of_exactly_the_cap_reaches_the_child_whole() {
    let base = Base::new("stdin-seed-cap");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(60))).expect("a launch id");
    let (entries, entered) = std_mpsc::channel();
    start.hooks.entries = Some(entries);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut writer = pipes.writer().expect("the writer");
    assert!(pipes.writer().is_none(), "one writer per launch");
    let mut stdout = std::mem::replace(&mut pipes.stdout, tokio::sync::mpsc::channel(1).1);
    let drained = tokio::spawn(async move {
        let mut total = 0;
        while let Some(chunk) = stdout.recv().await {
            total += chunk.len();
            if total == 16_777_216 {
                break;
            }
        }
        total
    });
    writer
        .seed(&vec![b'a'; 16_777_216], far())
        .await
        .expect("queued whole");
    assert_eq!(writer.seed(b"x", far()).await, Err(StdinRefused::Second));
    assert_eq!(
        timeout(Duration::from_secs(60), drained)
            .await
            .expect("echoed")
            .expect("the drain ends"),
        16_777_216
    );
    writer.end();
    assert_eq!(end(launch).await, Teardown::Done);
    let entered: Vec<usize> = entered.try_iter().collect();
    assert_eq!(entered, vec![65_536; 256]);
    assert_eq!(echoed(&base).iter().sum::<usize>(), 16_777_216);
}

/// One byte over the cap is refused whole, and the refusal uses up the one seed.
#[tokio::test]
async fn a_seed_one_over_the_cap_queues_nothing() {
    let base = Base::new("stdin-seed-over");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (entries, entered) = std_mpsc::channel();
    start.hooks.entries = Some(entries);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut writer = pipes.writer().expect("the writer");
    assert_eq!(
        writer.seed(&vec![b'a'; 16_777_217], far()).await,
        Err(StdinRefused::TooLarge)
    );
    assert_eq!(writer.seed(b"x", far()).await, Err(StdinRefused::Second));
    writer.end();
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(entered.try_iter().count(), 0);
    assert_eq!(echoed(&base), Vec::<usize>::new());
}

/// The control lines share their own 4 KiB, apart from the seed; a seed one byte over a chunk
/// goes as two.
#[tokio::test]
async fn control_lines_have_their_own_cap_and_the_seed_goes_in_chunks() {
    let base = Base::new("stdin-control");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (entries, entered) = std_mpsc::channel();
    start.hooks.entries = Some(entries);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut writer = pipes.writer().expect("the writer");
    writer.control(&[b'c'; 4000], far()).await.expect("under");
    writer
        .control(&[b'c'; 96], far())
        .await
        .expect("exactly 4,096");
    assert_eq!(
        writer.control(b"c", far()).await,
        Err(StdinRefused::TooLarge)
    );
    writer
        .seed(&vec![b's'; 65_537], far())
        .await
        .expect("queued");
    let mut seen = Vec::new();
    while seen.len() < 4 {
        seen.push(entered.recv_timeout(WAIT).expect("admitted"));
    }
    assert_eq!(seen, [4000, 96, 65_536, 1]);
    writer.end();
    assert_eq!(end(launch).await, Teardown::Done);

    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let mut second = super::tests::pipes(&mut launch).await;
    let mut writer = second.writer().expect("the writer");
    assert_eq!(
        writer.control(&[b'c'; 4097], far()).await,
        Err(StdinRefused::TooLarge)
    );
    writer.end();
    assert_eq!(end(launch).await, Teardown::Done);
}

/// A deaf child: the writer waits for room and sees each end of its wait. Dropped while it
/// waits it queues nothing more, its deadline gives `Deadline`, and a revocation gives
/// `Revoked` once the pump lets go. A control wait that ends any of these ways charges none of
/// the control allowance, so a whole-allowance control is still taken after each.
#[tokio::test]
async fn a_full_stdin_wait_ends_on_drop_deadline_and_revocation() {
    let base = Base::new("stdin-full");
    let lane = found(&base, "deaf").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, write) = barrier();
    let (entries, entered) = std_mpsc::channel();
    start.hooks.write = Some(paused);
    start.hooks.entries = Some(entries);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let mut writer = pipes.writer().expect("the writer");
    let seed = vec![b's'; 6 * 65_536];
    assert!(
        timeout(Duration::from_millis(500), writer.seed(&seed, far()))
            .await
            .is_err(),
        "the sixth chunk waits"
    );
    assert!(write.reached_within(WAIT), "the first chunk was admitted");
    assert_eq!(writer.seed(b"x", far()).await, Err(StdinRefused::Second));
    let whole = [b'c'; MAX_CONTROL];
    let soon = Instant::now() + Duration::from_millis(300);
    assert_eq!(
        writer.control(&whole, soon).await,
        Err(StdinRefused::Deadline)
    );
    assert!(
        timeout(Duration::from_millis(300), writer.control(&whole, far()))
            .await
            .is_err(),
        "dropped while it waits"
    );
    let waiting = tokio::spawn(async move { writer.control(&whole, far()).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(launch.lease().revoke(), Revoked::WriteInFlight);
    write.release();
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(
        timeout(WAIT, waiting).await.expect("ends").expect("joined"),
        Err(StdinRefused::Revoked)
    );
    assert_eq!(entered.try_iter().collect::<Vec<_>>(), [65_536]);
}

/// T15, S4's Idle half: once `revoke()` returns `Idle`, a chunk the pump took just before it
/// never enters a write, while the teardown is held at the job and after.
#[tokio::test]
async fn after_an_idle_revocation_no_write_is_entered() {
    let base = Base::new("stdin-idle");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (held, termination) = barrier();
    let (taken, at_admission) = barrier();
    let (entries, entered) = std_mpsc::channel();
    start.hooks.terminate = Some(held);
    start.hooks.admit = Some(taken);
    start.hooks.entries = Some(entries);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    pipes
        .raw()
        .try_send(Input::Chunk(vec![b'a'; 10]))
        .expect("room");
    assert!(at_admission.reached_within(WAIT), "taken, not yet admitted");
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    at_admission.release();
    let teardown = tokio::spawn(end(launch));
    // Waited off this runtime, which serves the listener the teardown aborts first.
    let termination = tokio::task::spawn_blocking(move || {
        assert!(termination.reached_within(WAIT), "held at the job");
        termination
    })
    .await
    .expect("the wait");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(entered.try_iter().count(), 0, "while held");
    termination.release();
    assert_eq!(teardown.await.expect("ends"), Teardown::Done);
    assert_eq!(entered.try_iter().count(), 0, "after");
    assert_eq!(echoed(&base), Vec::<usize>::new());
}

/// §1: the id, deadline, binding and gate are the launch's own from admission, the same ones the
/// worker and the listener use.
#[tokio::test]
async fn a_launch_carries_its_id_deadline_binding_and_gate_from_admission() {
    let base = Base::new("launch-handles");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    assert_eq!(launch.deadline(), start.deadline);
    launch.binding().bind(RunId(7)).expect("unbound");
    assert!(start.binding.bind(RunId(8)).is_err(), "one binding");
    launch.gate().pass();
    assert!(start.control.gate.may_call(), "one gate");
    let (ids, id) = std_mpsc::channel();
    start.hooks.id = Some(ids);
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    assert_eq!(id.recv_timeout(WAIT).expect("drawn"), launch.id());
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    assert_eq!(end(launch).await, Teardown::Done);
}
