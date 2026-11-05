//! The real speech worker's failures: each fault fixture costs the worker, never the brain, and
//! the next worker serves. Its own binary, like `screen_logs.rs`, so the Rez-only throw fixture's
//! `cl` run can't overlap a worker spawn in another binary's tests.
#![cfg(all(windows, debug_assertions))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kumochi_brain::platform::Job;
use kumochi_brain::speech::{
    Deadlines, NativeError, SpeechError, SpeechFault, SpeechHandle, SpeechSupervisor, WorkerLost,
};

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");

/// Generous budgets, except a short decode so the hang fixture ends quickly.
const FIXTURE: Deadlines = Deadlines {
    load: Duration::from_secs(20),
    decode: Duration::from_secs(3),
    decode_per_mille: 0,
    unload: Duration::from_secs(10),
};

fn start(job: &Arc<Job>) -> SpeechHandle {
    SpeechSupervisor::with_deadlines(PathBuf::from(WORKER), Arc::clone(job), FIXTURE)
        .expect("supervisor")
}

/// A load with no models is answered with a typed failure, so the worker serves: `Missing` when
/// no natives were ever staged, `Create` when an earlier run staged them.
async fn serves(speech: &SpeechHandle) {
    let nowhere = std::env::temp_dir().join("kumochi-no-natives-here");
    let loaded = speech.load(nowhere.clone(), nowhere).await;
    assert!(
        matches!(
            loaded,
            Err(SpeechError::Native(
                NativeError::Missing | NativeError::Create
            ))
        ),
        "a typed answer, not a loss: {loaded:?}"
    );
}

#[tokio::test]
async fn a_fresh_worker_answers_without_a_model() {
    let job = Arc::new(Job::new().expect("job"));
    let speech = start(&job);
    assert_eq!(
        speech.decode(16_000, &[0; 1_600]).await,
        Err(SpeechError::NotLoaded)
    );
    assert_eq!(speech.unload().await, Ok(()));
    assert_eq!(
        speech.decode(16_000, &[]).await,
        Err(SpeechError::Input),
        "refused before anything is sent"
    );
    serves(&speech).await;
    drop(speech);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[tokio::test]
async fn every_fault_loses_only_the_worker_and_the_next_one_serves() {
    let job = Arc::new(Job::new().expect("job"));
    for fault in [
        SpeechFault::Hang,
        SpeechFault::Crash,
        SpeechFault::Abort,
        SpeechFault::AccessViolation,
        SpeechFault::Garbage,
        SpeechFault::Panic,
    ] {
        let speech = start(&job);
        let mut losses = speech.lost();
        let (faulted, queued) = tokio::join!(
            speech.fault(fault.clone()),
            speech.decode(16_000, &[0; 1_600])
        );
        assert_eq!(faulted, Err(SpeechError::WorkerLost), "{fault:?}");
        assert_eq!(queued, Err(SpeechError::WorkerLost), "{fault:?} queued");
        assert_eq!(losses.recv().await.expect("loss"), WorkerLost);
        serves(&speech).await;
        drop(speech);
    }
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

/// Builds a DLL whose `kumochi_test_throw` throws, with the same compiler as the layout probe.
#[expect(
    clippy::disallowed_methods,
    reason = "the only test in this binary that spawns outside a worker, run alone with --exact"
)]
fn throwing_dll(dir: &std::path::Path) -> PathBuf {
    std::fs::create_dir_all(dir).expect("fixture folder");
    std::fs::write(
        dir.join("throw.cpp"),
        "#include <stdexcept>\n\
         extern \"C\" __declspec(dllexport) void kumochi_test_throw() {\n\
         throw std::runtime_error(\"private native fault text\");\n}\n",
    )
    .expect("source written");
    let built = std::process::Command::new("cl")
        .current_dir(dir)
        .args(["/nologo", "/EHs", "/LD", "throw.cpp", "/Fe:throw.dll"])
        .output()
        .expect("cl runs");
    assert!(built.status.success(), "cl failed: {built:?}");
    dir.join("throw.dll")
}

/// A C++ exception thrown through the worker's `C-unwind` boundary ends the worker; the brain gets
/// a content-free loss, and a fresh worker loads the real natives and Parakeet and decodes.
#[tokio::test]
#[ignore = "Rez only: needs cl, the pinned natives (SHERPA_NATIVES) and Parakeet (VOICE_MODELS)"]
async fn measure_a_native_throw_costs_only_the_worker() {
    let dir = std::env::temp_dir().join(format!("kumochi-throw-{}", std::process::id()));
    let dll = throwing_dll(&dir);
    let natives = PathBuf::from(std::env::var_os("SHERPA_NATIVES").expect("set SHERPA_NATIVES"));
    let models = PathBuf::from(std::env::var_os("VOICE_MODELS").expect("set VOICE_MODELS"));
    let job = Arc::new(Job::new().expect("job"));
    let speech = start(&job);
    let mut losses = speech.lost();
    assert_eq!(speech.load(natives.clone(), models.clone()).await, Ok(()));
    let thrown = std::time::Instant::now();
    assert_eq!(
        speech.fault(SpeechFault::Throw(dll)).await,
        Err(SpeechError::WorkerLost),
        "not a reply: the DLL loaded and the call didn't return"
    );
    assert!(
        thrown.elapsed() < FIXTURE.decode,
        "the worker ended, not the deadline"
    );
    assert_eq!(losses.recv().await.expect("loss"), WorkerLost);
    assert_eq!(
        speech.decode(16_000, &vec![0; 16_000]).await,
        Err(SpeechError::NotLoaded),
        "the model went with the worker"
    );
    assert_eq!(speech.load(natives, models).await, Ok(()));
    assert!(speech.decode(16_000, &vec![0; 16_000]).await.is_ok());
    drop(speech);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
    std::fs::remove_dir_all(dir).ok();
}
