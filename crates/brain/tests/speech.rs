//! The real speech worker's failures: each fault fixture costs the worker, never the brain, and
//! the next worker serves. Its own binary, like `screen_logs.rs`, so the Rez-only throw fixture's
//! `cl` run can't overlap a worker spawn in another binary's tests.
#![cfg(all(windows, debug_assertions))]

#[cfg(test)]
#[path = "../../../tests/support/scratch.rs"]
mod scratch;

use std::io::{self, Read as _};
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use kumochi_brain::platform::{Job, Launch, Stdio, spawn};
use kumochi_brain::speech::{
    Deadlines, Generation, NativeError, SpeechError, SpeechFault, SpeechHandle, SpeechSupervisor,
};
use kumochi_protocol::constants::{LOCAL_APPDATA_OVERRIDE, ROAMING_APPDATA_OVERRIDE};
use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

const WORKER: &str = env!("CARGO_BIN_EXE_kumochid");
const FIXTURE_PATIENCE: Duration = Duration::from_secs(180);
const OUTPUT_PATIENCE: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: u64 = 64 * 1024;

/// Generous budgets, except a short decode so the hang fixture ends quickly.
const FIXTURE: Deadlines = Deadlines {
    load: Duration::from_secs(20),
    decode: Duration::from_secs(3),
    decode_per_mille: 0,
    unload: Duration::from_secs(10),
};

fn drain(pipe: impl io::Read + Send + 'static) -> io::Result<mpsc::Receiver<io::Result<Vec<u8>>>> {
    let (send, receive) = mpsc::channel();
    drop(std::thread::Builder::new().spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        send.send(result).ok();
    })?);
    Ok(receive)
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "a completion pairs the exit result with whether the readers started"
)]
fn fixture_completion(result: io::Result<u32>, readers_started: bool) -> Result<(), String> {
    let code =
        result.map_err(|error| format!("speech fixture operation failed ({:?})", error.kind()))?;
    if code != 0 {
        return Err(format!("speech fixture exited with code {code}"));
    }
    if !readers_started {
        return Err("speech fixture output readers missing".into());
    }
    Ok(())
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the caller either confirmed teardown or must preserve the scratch"
)]
fn scratch_after_teardown(
    dir: ManuallyDrop<scratch::Scratch>,
    confirmed: bool,
) -> Option<scratch::Scratch> {
    // Leave scratch in place unless the child teardown is confirmed.
    confirmed.then(|| ManuallyDrop::into_inner(dir))
}

#[test]
fn unconfirmed_teardown_preserves_scratch() {
    let parent = scratch::dir("speech-teardown-test");
    let dir = ManuallyDrop::new(scratch::Scratch::under(parent.path(), "fixture"));
    let marker = dir.join("marker");
    std::fs::write(&marker, b"fixture evidence").expect("marker");

    drop(scratch_after_teardown(dir, false));
    assert_eq!(
        std::fs::read(marker).expect("marker survives"),
        b"fixture evidence"
    );
}

fn run_fixture(fixture: &str) {
    let exe = std::env::current_exe().expect("test binary");
    let system_root = std::env::var_os("SystemRoot").expect("Windows system root");
    let local = kumochi_storage::known_folder(FOLDERID_LocalAppData).expect("local app data");
    let dir = scratch::Scratch::under(&local, "speech");
    for half in ["local", "roaming"] {
        std::fs::create_dir(dir.join(half)).expect("an override half");
    }
    let job = Job::new().expect("fixture job");
    let dir = ManuallyDrop::new(dir);
    let mut stdout = None;
    let mut stderr = None;
    let result = (|| -> io::Result<u32> {
        let args = ["--exact".into(), fixture.into(), "--ignored".into()];
        let env = [
            ("SystemRoot".into(), system_root),
            (LOCAL_APPDATA_OVERRIDE.into(), dir.join("local").into()),
            (ROAMING_APPDATA_OVERRIDE.into(), dir.join("roaming").into()),
            ("TEMP".into(), dir.as_os_str().to_owned()),
            ("TMP".into(), dir.as_os_str().to_owned()),
            ("RUST_TEST_THREADS".into(), "1".into()),
        ];
        let mut child = spawn(&Launch {
            exe: &exe,
            args: &args,
            cwd: dir.path(),
            env: &env,
            stdio: Stdio::Piped,
            jobs: &[&job],
        })?;
        drop(child.stdin.take());
        stdout =
            Some(drain(child.stdout.take().ok_or_else(|| {
                io::Error::other("fixture stdout missing")
            })?)?);
        stderr =
            Some(drain(child.stderr.take().ok_or_else(|| {
                io::Error::other("fixture stderr missing")
            })?)?);
        if !child.process.exits_within(FIXTURE_PATIENCE)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "speech fixture exceeded its deadline",
            ));
        }
        child.process.exit_code()
    })();
    let stopped = job.terminate();
    let stdout = stdout.map(|receive| receive.recv_timeout(OUTPUT_PATIENCE));
    let stderr = stderr.map(|receive| receive.recv_timeout(OUTPUT_PATIENCE));
    let drained =
        stdout.as_ref().is_none_or(Result::is_ok) && stderr.as_ref().is_none_or(Result::is_ok);
    let dir = scratch_after_teardown(dir, stopped.is_ok() && drained);
    if let Err(error) = fixture_completion(result, stdout.is_some() && stderr.is_some()) {
        panic!(
            "{error}; job_stopped={}; output_drained={drained}",
            stopped.is_ok()
        );
    }
    let dir = dir.unwrap_or_else(|| {
        panic!(
            "fixture teardown incomplete; job_stopped={}; output_drained={drained}",
            stopped.is_ok()
        );
    });
    let path = dir.path().to_path_buf();
    drop(dir);
    assert!(
        !path.try_exists().expect("check cleanup"),
        "fixture directory left behind"
    );
    let stdout = stdout
        .expect("fixture stdout reader started")
        .expect("fixture stdout closed within its deadline")
        .unwrap_or_else(|error| panic!("fixture stdout read failed ({:?})", error.kind()));
    let stderr = stderr
        .expect("fixture stderr reader started")
        .expect("fixture stderr closed within its deadline")
        .unwrap_or_else(|error| panic!("fixture stderr read failed ({:?})", error.kind()));
    assert!(
        u64::try_from(stdout.len()).expect("bounded stdout") <= OUTPUT_LIMIT
            && u64::try_from(stderr.len()).expect("bounded stderr") <= OUTPUT_LIMIT,
        "fixture output exceeded its bound"
    );
    assert!(
        std::str::from_utf8(&stdout)
            .expect("test output is UTF-8")
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "the exact fixture ran and passed, rather than matching no tests"
    );
}

fn start(job: &Arc<Job>) -> SpeechHandle {
    SpeechSupervisor::with_deadlines(PathBuf::from(WORKER), Arc::clone(job), FIXTURE)
        .expect("supervisor")
}

async fn serves(speech: &SpeechHandle) {
    assert_eq!(
        speech.load("\0".into(), "\0".into()).await,
        Err(SpeechError::Native(NativeError::Input)),
        "the worker refuses the model path before native storage"
    );
}

#[test]
fn a_fresh_worker_answers_without_a_model() {
    run_fixture("fresh_worker_fixture");
}

#[tokio::test]
#[ignore = "exact child fixture with private logs, run by the default wrapper"]
async fn fresh_worker_fixture() {
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

#[test]
fn every_fault_loses_only_the_worker_and_the_next_one_serves() {
    run_fixture("faults_fixture");
}

#[tokio::test]
#[ignore = "exact child fixture with private logs, run by the default wrapper"]
async fn faults_fixture() {
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
        assert_eq!(losses.recv().await.expect("loss"), Generation(0));
        serves(&speech).await;
        drop(speech);
    }
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

/// The installer's one reload after a lost decode, sent the moment the decode fails and before
/// the loss is seen, as speech input does, is held through the exit wait and served by the next
/// worker.
#[tokio::test]
#[ignore = "exact child fixture with private logs, run by the default wrapper"]
async fn reload_fixture() {
    let job = Arc::new(Job::new().expect("job"));
    let speech = start(&job);
    let mut losses = speech.lost();
    assert_eq!(
        speech.fault(SpeechFault::Crash).await,
        Err(SpeechError::WorkerLost)
    );
    serves(&speech).await;
    assert_eq!(
        losses.try_recv(),
        Ok(Generation(0)),
        "served after the loss"
    );
    drop(speech);
    tokio::task::yield_now().await;
    job.terminate().expect("end job");
}

#[test]
fn a_reload_sent_as_a_decode_is_lost_is_served_by_the_next_worker() {
    run_fixture("reload_fixture");
}

/// Builds a DLL whose `kumochi_test_throw` throws, with the same compiler as the layout probe.
#[expect(
    clippy::disallowed_methods,
    reason = "the only test in this binary that spawns outside a worker, run alone with --exact"
)]
fn throwing_dll(dir: &std::path::Path) -> PathBuf {
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
    let dir = scratch::dir("speech-throw");
    let dll = throwing_dll(dir.path());
    let natives = PathBuf::from(std::env::var_os("SHERPA_NATIVES").expect("set SHERPA_NATIVES"));
    let models = PathBuf::from(std::env::var_os("VOICE_MODELS").expect("set VOICE_MODELS"));
    let job = Arc::new(Job::new().expect("job"));
    let dir = ManuallyDrop::new(dir);
    let speech = start(&job);
    let mut losses = speech.lost();
    assert_eq!(
        speech.load(natives.clone(), models.clone()).await,
        Ok(Generation(0))
    );
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
    assert_eq!(losses.recv().await.expect("loss"), Generation(0));
    assert_eq!(
        speech.decode(16_000, &vec![0; 16_000]).await,
        Err(SpeechError::NotLoaded),
        "the model went with the worker"
    );
    assert_eq!(speech.load(natives, models).await, Ok(Generation(1)));
    assert!(speech.decode(16_000, &vec![0; 16_000]).await.is_ok());
    drop(speech);
    tokio::task::yield_now().await;
    let stopped = job.terminate();
    drop(scratch_after_teardown(dir, stopped.is_ok()));
    stopped.expect("end job");
}
