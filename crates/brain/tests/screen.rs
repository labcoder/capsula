//! Real screen-worker lifecycle and content-free panic evidence.
#![cfg(all(windows, debug_assertions))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kumochi_brain::platform::Job;
use kumochi_brain::screen::{
    ReplyBody, RequestBody, ScreenError, ScreenSupervisor, WorkerFault, WorkerLost,
};

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");
const PATIENCE: Duration = Duration::from_secs(10);

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
    for fault in [WorkerFault::Crash, WorkerFault::Garbage] {
        assert_eq!(
            screen.request(RequestBody::Fault(fault)).await,
            Err(ScreenError::WorkerLost)
        );
        assert_eq!(losses.recv().await.expect("loss"), WorkerLost);
        assert_eq!(screen.request(RequestBody::Ping).await, Ok(ReplyBody::Pong));
    }
    assert_eq!(
        screen.request(RequestBody::Fault(WorkerFault::Crash)).await,
        Err(ScreenError::WorkerLost)
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
        WorkerLost
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

#[test]
fn panic_hook_and_environment_fault_are_exercised_in_child_processes() {
    let dir = std::env::temp_dir().join(format!("screen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("logs directory");
    for (test, fault) in [
        ("panic_fixture", "panic"),
        ("environment_crash_fixture", "crash"),
    ] {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env(kumochi_protocol::constants::SCREEN_FAULT_ENV, fault)
            .env("LOCALAPPDATA", &dir)
            .output()
            .expect("fixture process");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let logs = std::fs::read_dir(dir.join(kumochi_protocol::constants::DATA_DIR).join("logs"))
        .expect("logs")
        .map(|entry| std::fs::read_to_string(entry.expect("entry").path()).expect("log"))
        .collect::<String>();
    assert!(logs.contains("worker.rs:"), "{logs}");
    assert!(!logs.contains("private screen fault payload"), "{logs}");
    assert!(!logs.contains("RequestBody"), "{logs}");
    std::fs::remove_dir_all(dir).expect("remove logs");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn panic_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    drop(screen);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
#[ignore = "subprocess fixture with isolated environment"]
async fn environment_crash_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let screen = ScreenSupervisor::with_deadline(PathBuf::from(WORKER), Arc::clone(&job), PATIENCE)
        .expect("supervisor");
    let mut losses = screen.lost();
    tokio::time::pause();
    let keep_clock_manual = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for _ in 0..3 {
        assert_eq!(
            screen.request(RequestBody::Ping).await,
            Err(ScreenError::WorkerLost)
        );
        assert_eq!(losses.recv().await.expect("worker loss"), WorkerLost);
    }
    assert_eq!(
        screen.request(RequestBody::Ping).await,
        Err(ScreenError::WorkerLost)
    );
    assert_eq!(
        losses.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
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
