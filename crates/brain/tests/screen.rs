//! Real screen-worker lifecycle; the panic evidence lives in `screen_logs.rs`.
#![cfg(all(windows, debug_assertions))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kumochi_brain::platform::Job;
use kumochi_brain::screen::{
    Generation, ReplyBody, RequestBody, ScreenError, ScreenSupervisor, WorkerFault,
};

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");
const PATIENCE: Duration = Duration::from_secs(10);

/// Waits for `next` under [`PATIENCE`] of real time, which a paused tokio clock can't measure.
async fn within_patience<T>(next: impl std::future::Future<Output = T>) -> T {
    let (expire, expired) = tokio::sync::oneshot::channel::<()>();
    drop(std::thread::spawn(move || {
        std::thread::sleep(PATIENCE);
        drop(expire);
    }));
    tokio::select! {
        value = next => value,
        _ = expired => panic!("nothing within {PATIENCE:?}"),
    }
}

#[tokio::test]
async fn ping_loss_respawn_and_fourth_loss_backoff() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    tokio::time::pause();
    let keep_clock_manual = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for (fault, dead) in [(WorkerFault::Crash, 0), (WorkerFault::Garbage, 1)] {
        assert_eq!(
            screen.request(RequestBody::Fault(fault)).await,
            Err(ScreenError::WorkerLost)
        );
        assert_eq!(
            within_patience(losses.recv()).await.expect("loss"),
            Generation(dead)
        );
        assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    }
    assert_eq!(
        screen.request(RequestBody::Fault(WorkerFault::Crash)).await,
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(
        within_patience(losses.recv()).await.expect("third loss"),
        Generation(2)
    );
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    tokio::time::advance(Duration::from_millis(201)).await;
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    assert_eq!(
        screen.request(RequestBody::Fault(WorkerFault::Crash)).await,
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(
        within_patience(losses.recv()).await.expect("fourth loss"),
        Generation(3)
    );
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    tokio::time::advance(Duration::from_millis(999)).await;
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    tokio::time::advance(Duration::from_millis(2)).await;
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    keep_clock_manual.abort();
    assert!(
        keep_clock_manual
            .await
            .expect_err("manual clock task cancelled")
            .is_cancelled()
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
#[ignore = "quiet-Rez timing: 1,500–1,600 ms kill window"]
async fn hang_uses_one_absolute_deadline_and_kills_all_pending_work() {
    let job = Arc::new(Job::new().expect("job"));
    let screen =
        ScreenSupervisor::start(PathBuf::from(WORKER), Arc::clone(&job)).expect("supervisor");
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    let mut losses = screen.lost();
    let at = std::time::Instant::now();
    let (hung, queued) = tokio::join!(
        screen.request(RequestBody::Fault(WorkerFault::Hang)),
        screen.request(RequestBody::Ping)
    );
    assert_eq!(hung, Err(ScreenError::WorkerLost));
    assert_eq!(queued, Err(ScreenError::WorkerLost));
    assert_eq!(
        losses.recv().await.expect("termination notification"),
        Generation(0)
    );
    let elapsed = at.elapsed();
    assert!(
        (Duration::from_millis(1500)..Duration::from_millis(1600)).contains(&elapsed),
        "{elapsed:?}"
    );
    eprintln!("hang terminated after {elapsed:?}");
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
async fn injected_deadline_does_not_restart_for_queued_work() {
    use std::future::Future as _;
    use std::task::{Context, Poll, Waker};
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    tokio::time::pause();
    let keep_clock_manual = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let mut context = Context::from_waker(Waker::noop());
    let mut hung = Box::pin(screen.request(RequestBody::Fault(WorkerFault::Hang)));
    assert_eq!(hung.as_mut().poll(&mut context), Poll::Pending);
    tokio::time::advance(
        PATIENCE
            .checked_sub(Duration::from_millis(1))
            .expect("one ms under deadline"),
    )
    .await;
    assert_eq!(hung.as_mut().poll(&mut context), Poll::Pending);
    let mut queued = Box::pin(screen.request(RequestBody::Ping));
    assert_eq!(queued.as_mut().poll(&mut context), Poll::Pending);
    tokio::time::advance(Duration::from_millis(2)).await;
    tokio::time::resume();
    assert_eq!(hung.await, Err(ScreenError::WorkerLost));
    assert_eq!(queued.await, Err(ScreenError::WorkerLost));
    assert_eq!(
        within_patience(losses.recv()).await.expect("loss"),
        Generation(0)
    );
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    keep_clock_manual.abort();
    assert!(
        keep_clock_manual
            .await
            .expect_err("manual clock task cancelled")
            .is_cancelled()
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[test]
fn closing_the_job_ends_a_piped_worker() {
    let job = Job::new().expect("job");
    let (child, pipes) = Job::spawn_piped(
        std::path::Path::new(WORKER),
        kumochi_brain::SCREEN_WORKER,
        &[&job],
    )
    .expect("worker");
    assert!(!child.exits_within(Duration::ZERO).expect("wait"));
    drop(job);
    assert!(child.exits_within(PATIENCE).expect("wait"));
    drop(pipes);
}

#[tokio::test]
async fn an_idle_worker_killed_from_outside_is_reported_and_replaced() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    job.terminate().expect("kill the idle worker");
    let loss = tokio::time::timeout(PATIENCE, losses.recv()).await;
    assert_eq!(
        loss.expect("lost with nothing pending").expect("loss"),
        Generation(0)
    );
    let started = std::time::Instant::now();
    let replacement = screen.request(RequestBody::Ping).await;
    assert_eq!(
        replacement,
        Ok(ReplyBody::Pong),
        "replacement ping elapsed {:?}, injected request deadline {:?}",
        started.elapsed(),
        PATIENCE
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
async fn the_first_request_after_a_loss_reaches_the_next_worker() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    for dead in 0..2 {
        assert_eq!(
            screen.request(RequestBody::Fault(WorkerFault::Crash)).await,
            Err(ScreenError::WorkerLost)
        );
        assert_eq!(
            within_patience(losses.recv()).await.expect("loss"),
            Generation(dead)
        );
        assert_eq!(
            screen.request(RequestBody::Ping).await,
            Ok(ReplyBody::Pong),
            "the loss is the recovery boundary, so one request with no retry is served"
        );
    }
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}
