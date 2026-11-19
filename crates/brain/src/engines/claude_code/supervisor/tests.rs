//! Question launches on the lane's fake `claude.exe` (`lane::tests::fake`), in a scratch base:
//! no network, no account and no credential store.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::ENGINE_DIR;
use kumochi_protocol::message::{RunId, ThreadId};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{
    CHUNK, Input, Launch, LaunchFailed, LaunchId, MAX_LAUNCH, PIPE_QUEUE, Pipes, Question, Revoked,
    Start, Teardown, admit, mcp_json,
};
use crate::bridge::listener::Listener;
use crate::coordinator::Input as CoordinatorInput;
use crate::engines::claude_code::lane::tests::{Base, host, locked, soon};
use crate::engines::claude_code::lane::{Failure, LANE_QUEUE, Lane, Outcome, PIPE_GRACE, Work};
use crate::engines::{self, Asks, Engine, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::platform::sherpa::tests::set_junction;
use crate::platform::{self, FAKE_CLAUDE, Job, Stdio};

/// What a test sets on a launch before it's queued.
#[derive(Default)]
pub(crate) struct Hooks {
    /// Where the stdin pump waits, with its first chunk taken and not yet admitted.
    pub(crate) admit: Option<Pause>,
    /// Where the stdin pump waits, between its first admission and its write.
    pub(crate) write: Option<Pause>,
    /// Hears the length of each chunk written to the child's stdin.
    pub(crate) writes: Option<std_mpsc::Sender<usize>>,
    /// Hears the length of each chunk admitted, before its write starts.
    pub(crate) entries: Option<std_mpsc::Sender<usize>>,
    /// Where the start waits, once the launch's folder is made and before its listener.
    pub(crate) folder: Option<Pause>,
    /// Hears the launch's id once it's drawn.
    pub(crate) id: Option<std_mpsc::Sender<LaunchId>>,
    /// Hears each pump's name as its thread ends.
    pub(crate) pumps: Option<std_mpsc::Sender<&'static str>>,
    /// The pump whose thread fails to start.
    pub(crate) spawn_fails: Option<&'static str>,
    /// Where the teardown waits, once the listener is aborted and before it ends the job.
    pub(crate) terminate: Option<Pause>,
}

/// The pumps' share of [`Hooks`].
pub(super) struct Pumps {
    pub(super) stdin: Stdin,
    pub(super) report: Option<std_mpsc::Sender<&'static str>>,
    pub(super) fails: Option<&'static str>,
}

/// The stdin pump's share of [`Hooks`].
pub(super) struct Stdin {
    pub(super) admit: Option<Pause>,
    pub(super) write: Option<Pause>,
    pub(super) writes: Option<std_mpsc::Sender<usize>>,
    pub(super) entries: Option<std_mpsc::Sender<usize>>,
}

/// The launch's side of a barrier.
pub(crate) struct Pause {
    reached: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
}

/// The test's side of a barrier.
pub(crate) struct Barrier {
    reached: std_mpsc::Receiver<()>,
    release: std_mpsc::Sender<()>,
}

impl Barrier {
    pub(crate) fn reached_within(&self, wait: Duration) -> bool {
        self.reached.recv_timeout(wait).is_ok()
    }

    pub(crate) fn release(&self) {
        self.release.send(()).ok();
    }
}

pub(crate) fn barrier() -> (Pause, Barrier) {
    let (reached, reached_rx) = std_mpsc::channel();
    let (release_tx, release) = std_mpsc::channel();
    (
        Pause { reached, release },
        Barrier {
            reached: reached_rx,
            release: release_tx,
        },
    )
}

/// Waits at `pause` until the test releases it, or the test is gone.
pub(super) fn pause(pause: Option<Pause>) {
    if let Some(pause) = pause {
        pause.reached.send(()).ok();
        pause.release.recv().ok();
    }
}

/// A pump's end signal that also tells the test which pump ended.
pub(super) struct Ending {
    _signal: std_mpsc::Sender<()>,
    report: Option<(std_mpsc::Sender<&'static str>, &'static str)>,
}

impl Ending {
    pub(super) fn new(
        signal: std_mpsc::Sender<()>,
        name: &'static str,
        report: Option<std_mpsc::Sender<&'static str>>,
    ) -> Self {
        Self {
            _signal: signal,
            report: report.map(|report| (report, name)),
        }
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        if let Some((report, name)) = self.report.take() {
            report.send(name).ok();
        }
    }
}

pub(crate) const WAIT: Duration = Duration::from_secs(10);

pub(crate) fn question(lifetime: Duration) -> Question {
    Question {
        thread: ThreadId(1),
        model: "claude-test".to_owned(),
        effort: None,
        tools: Arc::default(),
        lifetime,
        calls: None,
    }
}

/// A lane on the fake in `mode` that has found the install.
pub(crate) async fn found(base: &Base, mode: &str) -> Lane {
    found_with(base, mode, |_| {}).await
}

async fn found_with(
    base: &Base,
    mode: &str,
    change: impl FnOnce(&mut crate::engines::claude_code::lane::Host),
) -> Lane {
    let (mut host, _switches) = host(base, mode);
    change(&mut host);
    let mut lane = Lane::start(host).expect("lane");
    assert!(lane.submit(0, Work::Version), "admitted");
    let done = timeout(WAIT, lane.next()).await.expect("the lane answers");
    assert!(matches!(done.result, Ok(Outcome::Version(_))), "{done:?}");
    lane
}

pub(crate) async fn version(lane: &mut Lane) -> Result<Outcome, Failure> {
    assert!(lane.submit(0, Work::Version), "admitted");
    timeout(WAIT, lane.next())
        .await
        .expect("the lane answers")
        .result
}

pub(crate) fn queue(lane: &Lane, start: Start) {
    assert!(lane.launch(start), "queued");
}

pub(crate) async fn pipes(launch: &mut Launch) -> Pipes {
    timeout(WAIT, launch.started())
        .await
        .expect("answered")
        .expect("the first answer")
        .expect("started")
}

pub(crate) async fn failed(launch: &mut Launch) -> LaunchFailed {
    match timeout(WAIT, launch.started())
        .await
        .expect("answered")
        .expect("the first answer")
    {
        Ok(_) => panic!("the launch started"),
        Err(failed) => failed,
    }
}

pub(crate) async fn end(launch: Launch) -> Teardown {
    timeout(Duration::from_secs(20), launch.end())
        .await
        .expect("the teardown ends")
}

fn engine_dir(base: &Base) -> PathBuf {
    base.owned().join(ENGINE_DIR)
}

/// Every path under the engine folder with its size, sorted.
fn tree(base: &Base) -> Vec<(PathBuf, u64)> {
    fn walk(dir: &Path, found: &mut Vec<(PathBuf, u64)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or_default();
            found.push((path.clone(), size));
            walk(&path, found);
        }
    }
    let mut found = Vec::new();
    walk(&engine_dir(base), &mut found);
    found.sort();
    found
}

/// The launch folders under the engine folder.
pub(crate) fn launch_dirs(base: &Base) -> Vec<String> {
    fs::read_dir(engine_dir(base))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn questions(base: &Base) -> usize {
    base.calls()
        .iter()
        .filter(|call| call.starts_with("-p "))
        .count()
}

/// The lengths the echo fake read, in order.
pub(crate) fn echoed(base: &Base) -> Vec<usize> {
    fs::read_to_string(base.0.join("echo.log"))
        .map(|log| {
            log.lines()
                .map(|line| line.parse().expect("a length"))
                .collect()
        })
        .unwrap_or_default()
}

/// Waits until `check` holds.
pub(crate) async fn until(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Owner-only folders down to `path` under `base`, as a launch makes them.
fn owned_dirs(base: &Base, path: &Path) {
    let relative = path.strip_prefix(base.0.path()).expect("under the base");
    let mut dir = base.0.path().to_path_buf();
    for part in relative.components() {
        dir.push(part);
        drop(platform::owner_only_dir(&dir).expect("an owner-only folder"));
    }
}

fn leftover(base: &Base, hex: &str) -> PathBuf {
    let work = engine_dir(base).join(hex).join("work");
    owned_dirs(base, &work);
    let file = work.join("f");
    fs::write(&file, b"left").expect("a leftover file");
    file
}

fn slow_terminate(job: &Job) -> std::io::Result<()> {
    std::thread::sleep(Duration::from_millis(1500));
    job.terminate()
}

/// Set once a test's lane has found the install: from then on its terminations fail.
static TERMINATION_FAILS: AtomicBool = AtomicBool::new(false);

fn failed_terminate(job: &Job) -> std::io::Result<()> {
    if TERMINATION_FAILS.load(Ordering::SeqCst) {
        return Err(std::io::Error::other("refused"));
    }
    job.terminate()
}

/// Reports the job empty while its child may still be exiting, as Windows does for the moment
/// between a process leaving its job and its handles closing.
#[expect(
    clippy::unnecessary_wraps,
    reason = "it stands in for `Job::terminate`"
)]
fn confirmed_early(_job: &Job) -> std::io::Result<()> {
    Ok(())
}

/// S13: every bound, against literals.
#[test]
fn the_bounds_hold() {
    assert_eq!(MAX_LAUNCH, Duration::from_secs(150));
    assert_eq!(PIPE_QUEUE, 4);
    assert_eq!(CHUNK, 65536);
    assert_eq!(LANE_QUEUE, 4);
}

/// The deadline is fixed once, at admission.
#[test]
fn a_launch_deadline_is_fixed_at_admission() {
    let before = Instant::now();
    let (start, _launch) = admit(question(Duration::from_secs(150))).expect("a launch id");
    let after = Instant::now();
    assert!(start.deadline >= before + Duration::from_secs(150));
    assert!(start.deadline <= after + Duration::from_secs(150));
}

/// S14.
#[test]
fn mcp_json_names_the_bridge_and_nothing_else() {
    let config = mcp_json(Path::new(r"C:\Kumochi\kumochid.exe"), LaunchId(0xab)).expect("text");
    // Compared as JSON: the workspace build turns on serde_json's preserve_order, so key order varies.
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&config).expect("JSON"),
        serde_json::json!({"mcpServers": {"kumochi": {
            "command": r"C:\Kumochi\kumochid.exe",
            "args": ["--mcp-bridge", "000000000000000000000000000000ab"],
        }}}),
    );
}

/// S15.
#[test]
fn input_debug_shows_no_chunk_bytes() {
    let marker = b"MARKER-7f3a".to_vec();
    let shown = format!("{:?}", Input::Chunk(marker.clone()));
    assert_eq!(shown, "Chunk { bytes: 11 }");
    let bytes = format!("{marker:?}");
    assert!(!shown.contains("MARKER") && !shown.contains(&bytes[1..bytes.len() - 1]));
}

/// S3.
#[test]
fn revocation_fails_the_gate_at_once() {
    let (start, launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    start.control.gate.pass();
    assert!(start.control.gate.may_call());
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    assert!(!start.control.gate.may_call());
    assert_eq!(launch.lease().revoke(), Revoked::Already);
}

/// S1.
#[tokio::test]
async fn dropping_the_cancel_tears_the_launch_down() {
    struct Holding(Mutex<Option<Launch>>);

    impl std::fmt::Debug for Holding {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Holding")
        }
    }

    impl Engine for Holding {
        fn start(&self, _: RunRequest, events: mpsc::Sender<TurnEvent>, _: SendGate) -> Round {
            let mut launch = self.0.lock().expect("held").take().expect("one round");
            Box::pin(async move {
                let _pipes = launch.started().await;
                // A round that drops its events has ended.
                std::future::pending::<()>().await;
                drop((launch, events));
            })
        }
    }

    let base = Base::new("launch-cancel");
    let mut brain_job = None;
    let lane = found_with(&base, "deaf", |host| {
        brain_job = Some(Arc::clone(&host.brain_job));
    })
    .await;
    let brain_job = brain_job.expect("the brain job");
    let held = base.0.join("sibling");
    let mut holding = std::ffi::OsString::from("hold:");
    holding.push(&held);
    let sibling = platform::spawn(&platform::Launch {
        exe: &std::env::current_exe().expect("exe"),
        args: &[FAKE_CLAUDE.into(), holding],
        cwd: base.0.path(),
        env: &[],
        stdio: Stdio::Null,
        jobs: &[&brain_job],
    })
    .expect("a sibling in the brain job");
    let (start, launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let engine = Holding(Mutex::new(Some(launch)));
    let (inputs, _received) = mpsc::channel(8);
    let (gate_inputs, _gated) = mpsc::unbounded_channel();
    let request = RunRequest::for_tests(RunId(1), ThreadId(1), "question");
    let gate = SendGate::new(request.key, Asks::default(), gate_inputs);
    let cancel = engines::start(&engine, request, inputs, gate);
    until("the launch runs", || questions(&base) == 1).await;
    assert_eq!(launch_dirs(&base).len(), 1);
    drop(cancel);
    until("the launch folder is gone", || {
        launch_dirs(&base).is_empty()
    })
    .await;
    assert_eq!(
        sibling.process.exits_within(Duration::ZERO).ok(),
        Some(false),
        "the sibling runs"
    );
    sibling.process.terminate().expect("the sibling ends");
}

/// S2.
#[tokio::test]
async fn finished_reaches_the_brain_before_teardown() {
    struct Answering(Mutex<Option<Launch>>);

    impl std::fmt::Debug for Answering {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Answering")
        }
    }

    impl Engine for Answering {
        fn start(&self, _: RunRequest, events: mpsc::Sender<TurnEvent>, _: SendGate) -> Round {
            let mut launch = self.0.lock().expect("held").take().expect("one round");
            Box::pin(async move {
                let _pipes = launch.started().await;
                events
                    .send(TurnEvent::Finished(Finish::Completed))
                    .await
                    .ok();
                drop(launch);
            })
        }
    }

    let base = Base::new("launch-finished");
    let lane = found_with(&base, "deaf", |host| host.terminate = slow_terminate).await;
    let (start, launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let engine = Answering(Mutex::new(Some(launch)));
    let (inputs, mut received) = mpsc::channel(8);
    let (gate_inputs, _gated) = mpsc::unbounded_channel();
    let request = RunRequest::for_tests(RunId(1), ThreadId(1), "question");
    let gate = SendGate::new(request.key, Asks::default(), gate_inputs);
    let _cancel = engines::start(&engine, request, inputs, gate);
    let input = timeout(WAIT, received.recv())
        .await
        .expect("an input")
        .expect("the pump");
    assert!(matches!(
        input,
        CoordinatorInput::Engine {
            event: TurnEvent::Finished(Finish::Completed),
            ..
        }
    ));
    assert_eq!(launch_dirs(&base).len(), 1, "the teardown hasn't run yet");
    until("the launch folder is gone", || {
        launch_dirs(&base).is_empty()
    })
    .await;
}

/// S4: a chunk taken but not yet admitted when the launch is revoked is never written, and chunks
/// queued behind an admitted write never are either. Writes are heard at the pipe; the second
/// case also holds the teardown before the job ends, so the child reads the admitted chunk.
#[tokio::test]
async fn no_chunk_admitted_after_revoke_reaches_the_child() {
    let base = Base::new("launch-admission");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (taken, at_admission) = barrier();
    let (writes, written) = std_mpsc::channel();
    start.hooks.admit = Some(taken);
    start.hooks.writes = Some(writes);
    queue(&lane, start);
    let first = pipes(&mut launch).await;
    first
        .raw()
        .try_send(Input::Chunk(vec![b'a'; 10]))
        .expect("room");
    assert!(at_admission.reached_within(WAIT), "the chunk was taken");
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    at_admission.release();
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(written.try_iter().collect::<Vec<_>>(), Vec::<usize>::new());

    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (held, termination) = barrier();
    let (paused, barrier) = barrier();
    let (writes, written) = std_mpsc::channel();
    start.hooks.write = Some(paused);
    start.hooks.writes = Some(writes);
    start.hooks.terminate = Some(held);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    pipes
        .raw()
        .try_send(Input::Chunk(vec![b'a'; 10]))
        .expect("room");
    assert!(barrier.reached_within(WAIT), "the first chunk was admitted");
    for _ in 0..PIPE_QUEUE {
        pipes
            .raw()
            .try_send(Input::Chunk(vec![b'b'; 20]))
            .expect("room");
    }
    assert_eq!(launch.lease().revoke(), Revoked::WriteInFlight);
    // Waited off this runtime, which serves the listener the teardown aborts first.
    let termination = tokio::task::spawn_blocking(move || {
        assert!(
            termination.reached_within(WAIT),
            "the teardown waits to end the job"
        );
        termination
    })
    .await
    .expect("the wait");
    barrier.release();
    assert_eq!(
        written.recv_timeout(WAIT),
        Ok(10),
        "the admitted write returned"
    );
    until("the child read the admitted chunk", || {
        echoed(&base) == [10]
    })
    .await;
    termination.release();
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(
        written.try_iter().collect::<Vec<_>>(),
        Vec::<usize>::new(),
        "no queued chunk was written"
    );
    assert_eq!(
        echoed(&base),
        [10],
        "the child read only the admitted chunk"
    );
}

/// M2: a pump thread that can't start fails the launch as `Spawn`, and the pumps already
/// started, the job, the listener and the folder are ended in order.
#[tokio::test]
async fn a_pump_that_fails_to_start_stops_the_launch() {
    let base = Base::new("launch-pump-fails");
    let lane = found(&base, "deaf").await;
    let cases: [(&str, &[&str]); 2] = [
        ("claude-code-stdout", &["claude-code-stdin"]),
        (
            "claude-code-stderr",
            &["claude-code-stdin", "claude-code-stdout"],
        ),
    ];
    for (fails, started) in cases {
        let (mut start, mut launch) =
            admit(question(Duration::from_secs(20))).expect("a launch id");
        let (id_tx, id) = std_mpsc::channel();
        let (pumps, ended) = std_mpsc::channel();
        start.hooks.id = Some(id_tx);
        start.hooks.pumps = Some(pumps);
        start.hooks.spawn_fails = Some(fails);
        queue(&lane, start);
        assert_eq!(failed(&mut launch).await, LaunchFailed::Spawn);
        assert_eq!(
            end(launch).await,
            Teardown::Done,
            "the job was confirmed empty"
        );
        let mut names: Vec<_> = ended.try_iter().collect();
        names.sort_unstable();
        assert_eq!(names, started, "the started pumps ended");
        let id = id.recv_timeout(WAIT).expect("the id");
        assert!(Listener::create(id).is_ok(), "the listener was ended");
        assert!(launch_dirs(&base).is_empty(), "the folder is gone");
    }
}

/// A child still exiting when its job reads empty still holds its working folder: the teardown
/// waits for the child itself before deleting the folder.
#[tokio::test]
async fn a_child_still_exiting_when_its_job_reads_empty_has_its_folder_deleted() {
    let base = Base::new("launch-exits-late");
    let lane = found_with(&base, "late", |host| host.terminate = confirmed_early).await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    start.hooks.spawn_fails = Some("claude-code-stdout");
    queue(&lane, start);
    assert_eq!(failed(&mut launch).await, LaunchFailed::Spawn);
    assert_eq!(end(launch).await, Teardown::Done, "the folder was deleted");
    assert!(launch_dirs(&base).is_empty(), "the folder is gone");
}

/// C-M1: a listener task the runtime can't drop within the grace leaves the launch `Stuck`, the
/// folder kept and the lane refusing.
#[tokio::test]
async fn a_listener_that_outlives_the_grace_is_stuck() {
    let base = Base::new("launch-listener-held");
    let mut lane = found(&base, "deaf").await;
    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    // The listener's task runs on this test's runtime, so blocking it withholds the abort.
    std::thread::sleep(PIPE_GRACE * 3);
    assert_eq!(end(launch).await, Teardown::Stuck);
    assert_eq!(launch_dirs(&base).len(), 1, "the folder is kept");
    assert_eq!(version(&mut lane).await, Err(Failure::Refused));
}

/// End closes stdin: the child reads its end and exits, which ends the launch.
#[tokio::test]
async fn end_closes_stdin_and_the_child_exits() {
    let base = Base::new("launch-end-input");
    let lane = found(&base, "echo").await;
    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let control = start.control();
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    pipes
        .raw()
        .try_send(Input::Chunk(b"hello".to_vec()))
        .expect("room");
    pipes.raw().try_send(Input::End).expect("room");
    let echoed_back = timeout(WAIT, pipes.stdout.recv()).await.expect("output");
    assert_eq!(echoed_back.as_deref(), Some(&b"hello"[..]));
    until("the child's exit ends it", || control.is_ended()).await;
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(echoed(&base), [5]);
}

/// S16 (i): a write admitted before the revocation is in flight, and settles before cleanup.
#[tokio::test]
async fn a_paused_admitted_write_is_in_flight_and_settles_before_cleanup() {
    let base = Base::new("launch-in-flight");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, barrier) = barrier();
    let (pumps, ended) = std_mpsc::channel();
    start.hooks.write = Some(paused);
    start.hooks.pumps = Some(pumps);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    pipes
        .raw()
        .try_send(Input::Chunk(vec![b'a'; 10]))
        .expect("room");
    assert!(barrier.reached_within(WAIT), "admitted");
    assert_eq!(launch.lease().revoke(), Revoked::WriteInFlight);
    let mut teardown = std::pin::pin!(launch.end());
    assert!(
        timeout(Duration::from_millis(500), &mut teardown)
            .await
            .is_err(),
        "the teardown waits for the write"
    );
    assert_eq!(launch_dirs(&base).len(), 1, "nothing is deleted yet");
    barrier.release();
    assert_eq!(timeout(WAIT, teardown).await.expect("ends"), Teardown::Done);
    let mut names: Vec<_> = ended.try_iter().collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "claude-code-stderr",
            "claude-code-stdin",
            "claude-code-stdout"
        ]
    );
    assert!(launch_dirs(&base).is_empty());
}

/// S16 (ii): a write held past the pipe grace leaves the launch stuck, with its folder.
#[tokio::test]
async fn a_write_held_past_the_grace_is_stuck() {
    let base = Base::new("launch-held-write");
    let mut lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, barrier) = barrier();
    start.hooks.write = Some(paused);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    pipes
        .raw()
        .try_send(Input::Chunk(vec![b'a'; 10]))
        .expect("room");
    assert!(barrier.reached_within(WAIT), "admitted");
    assert_eq!(end(launch).await, Teardown::Stuck);
    assert_eq!(launch_dirs(&base).len(), 1, "the folder is kept");
    assert_eq!(version(&mut lane).await, Err(Failure::Refused));
    barrier.release();
    assert!(!lane.close(soon()).await, "a stuck lane keeps its lock");
}

/// S16: a revocation before any admission: nothing is admitted.
#[tokio::test]
async fn a_revocation_before_admission_admits_nothing() {
    let base = Base::new("launch-before-admission");
    let lane = found(&base, "echo").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, barrier) = barrier();
    start.hooks.write = Some(paused);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    pipes.raw().try_send(Input::Chunk(vec![b'a'; 10])).ok();
    assert!(
        !barrier.reached_within(Duration::from_millis(300)),
        "never admitted"
    );
    assert_eq!(end(launch).await, Teardown::Done);
    assert_eq!(echoed(&base), Vec::<usize>::new());
}

/// S17: both pumps end with the round still holding both of its ends.
#[tokio::test]
async fn both_pumps_end_with_their_endpoints_still_held() {
    let base = Base::new("launch-endpoints");
    let lane = found(&base, "flood").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (pumps, ended) = std_mpsc::channel();
    start.hooks.pumps = Some(pumps);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    until("stdout is full", || pipes.stdout.len() == PIPE_QUEUE).await;
    assert_eq!(launch.lease().revoke(), Revoked::Idle);
    assert_eq!(end(launch).await, Teardown::Done);
    let mut names: Vec<_> = ended.try_iter().collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "claude-code-stderr",
            "claude-code-stdin",
            "claude-code-stdout"
        ]
    );
    drop(pipes);
}

/// The stdout bound: a round that never reads holds exactly four chunks of at most 64 KiB.
#[tokio::test]
async fn stdout_holds_four_chunks_for_a_round_that_never_reads() {
    let base = Base::new("launch-stdout-bound");
    let lane = found(&base, "flood").await;
    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    until("stdout is full", || pipes.stdout.len() == 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(pipes.stdout.len(), 4);
    assert_eq!(end(launch).await, Teardown::Done);
    let mut chunks = 0;
    while let Ok(chunk) = pipes.stdout.try_recv() {
        assert!(!chunk.is_empty() && chunk.len() <= 65536, "{}", chunk.len());
        chunks += 1;
    }
    assert_eq!(chunks, 4);
}

/// The stdin bound: a deaf child takes one chunk into its pipe and four wait; the next is
/// refused.
#[tokio::test]
async fn stdin_holds_four_chunks_for_a_deaf_child() {
    let base = Base::new("launch-stdin-bound");
    let lane = found(&base, "deaf").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, barrier) = barrier();
    start.hooks.write = Some(paused);
    queue(&lane, start);
    let pipes = pipes(&mut launch).await;
    let chunk = || Input::Chunk(vec![b'q'; 65536]);
    pipes.raw().try_send(chunk()).expect("room");
    assert!(
        barrier.reached_within(WAIT),
        "the pump holds the first chunk"
    );
    pipes.raw().try_send(chunk()).expect("room 1");
    pipes.raw().try_send(chunk()).expect("room 2");
    pipes.raw().try_send(chunk()).expect("room 3");
    pipes.raw().try_send(chunk()).expect("room 4");
    assert!(matches!(
        pipes.raw().try_send(chunk()),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    barrier.release();
    assert_eq!(end(launch).await, Teardown::Done);
}

/// The lane's queue: four launches wait behind a running one, and the next is refused.
#[tokio::test]
async fn the_lane_queue_holds_four_launches() {
    let base = Base::new("launch-lane-queue");
    let lane = found(&base, "deaf").await;
    let (start, mut first) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _pipes = pipes(&mut first).await;
    until("the first runs", || questions(&base) == 1).await;
    let mut waiting = Vec::new();
    for _ in 0..4 {
        let (start, launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
        queue(&lane, start);
        waiting.push(launch);
    }
    let (start, _refused) = admit(question(Duration::from_secs(20))).expect("a launch id");
    assert!(!lane.launch(start), "the fifth waits nowhere");
    for launch in &waiting {
        launch.lease().revoke();
    }
    assert_eq!(end(first).await, Teardown::Done);
    for mut launch in waiting {
        assert_eq!(failed(&mut launch).await, LaunchFailed::Revoked);
    }
    assert_eq!(questions(&base), 1);
}

/// One launch at a time: the second starts only after the first's teardown.
#[tokio::test]
async fn launches_run_one_at_a_time() {
    let base = Base::new("launch-serial");
    let lane = found(&base, "deaf").await;
    let (start, mut first) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _first_pipes = pipes(&mut first).await;
    until("the first runs", || questions(&base) == 1).await;
    let (start, mut second) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let second_control = start.control();
    queue(&lane, start);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(launch_dirs(&base).len(), 1, "the second waits");
    assert_eq!(end(first).await, Teardown::Done);
    let _second_pipes = pipes(&mut second).await;
    until("the second runs", || questions(&base) == 2).await;
    assert!(!second_control.is_ended());
    assert_eq!(questions(&base), 2);
    assert_eq!(end(second).await, Teardown::Done);
}

/// S5.
#[tokio::test]
async fn a_queued_launch_revoked_before_its_turn_never_starts() {
    let base = Base::new("launch-revoked-queued");
    let lane = found(&base, "deaf").await;
    let (start, mut first) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _pipes = pipes(&mut first).await;
    until("the first runs", || questions(&base) == 1).await;
    let (start, mut revoked) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    assert_eq!(revoked.lease().revoke(), Revoked::Idle);
    assert_eq!(end(first).await, Teardown::Done);
    assert_eq!(failed(&mut revoked).await, LaunchFailed::Revoked);
    assert_eq!(end(revoked).await, Teardown::Done);
    let (start, mut later) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _later_pipes = pipes(&mut later).await;
    until("the later one runs", || questions(&base) == 2).await;
    assert_eq!(end(later).await, Teardown::Done);
    assert_eq!(questions(&base), 2);
}

/// The deadline: an expired launch never starts, and a live one ends at its deadline.
#[tokio::test]
async fn the_deadline_is_kept_from_admission() {
    let base = Base::new("launch-deadline");
    let lane = found(&base, "deaf").await;
    let (start, mut zero) = admit(question(Duration::ZERO)).expect("a launch id");
    queue(&lane, start);
    assert_eq!(failed(&mut zero).await, LaunchFailed::Expired);
    let (start, mut first) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _pipes = pipes(&mut first).await;
    until("the first runs", || questions(&base) == 1).await;
    let (start, mut waiting) = admit(question(Duration::from_millis(300))).expect("a launch id");
    queue(&lane, start);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(end(first).await, Teardown::Done);
    assert_eq!(failed(&mut waiting).await, LaunchFailed::Expired);
    assert_eq!(questions(&base), 1);
    let (start, mut short) = admit(question(Duration::from_millis(300))).expect("a launch id");
    let control = start.control();
    queue(&lane, start);
    let _short_pipes = pipes(&mut short).await;
    until("the deadline ends it", || control.is_ended()).await;
    assert_eq!(end(short).await, Teardown::Done);
}

/// S12.
#[tokio::test]
async fn closing_ends_the_live_launch() {
    let base = Base::new("launch-close");
    let mut lane = found(&base, "deaf").await;
    let (start, mut live) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _pipes = pipes(&mut live).await;
    until("it runs", || questions(&base) == 1).await;
    let mut queued = Vec::new();
    for _ in 0..3 {
        let (start, launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
        let control = start.control();
        queue(&lane, start);
        queued.push((control, launch));
    }
    assert!(lane.close(soon()).await, "closed clean");
    assert_eq!(end(live).await, Teardown::Done);
    // The first is the one the worker takes as it sees the close; each is settled.
    for (control, mut launch) in queued {
        assert_eq!(
            failed(&mut launch).await,
            LaunchFailed::Lane(Failure::Refused)
        );
        assert_eq!(end(launch).await, Teardown::Done);
        assert!(control.is_ended(), "its control is ended");
    }
    assert!(launch_dirs(&base).is_empty());
    assert_eq!(questions(&base), 1);
}

/// S6: the child exits on its own, so its pipes end and only the failed termination keeps the
/// folder.
#[tokio::test]
async fn a_failed_termination_keeps_everything() {
    let base = Base::new("launch-terminate-fails");
    let mut brain_job = None;
    let mut lane = found_with(&base, "out", |host| {
        host.terminate = failed_terminate;
        brain_job = Some(Arc::clone(&host.brain_job));
    })
    .await;
    TERMINATION_FAILS.store(true, Ordering::SeqCst);
    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let control = start.control();
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    until("the exit ends it", || control.is_ended()).await;
    assert_eq!(end(launch).await, Teardown::Stuck);
    assert_eq!(launch_dirs(&base).len(), 1, "the folder is kept");
    assert_eq!(version(&mut lane).await, Err(Failure::Refused));
    assert_eq!(launch_dirs(&base).len(), 1, "and not swept");
    brain_job
        .expect("the brain job")
        .terminate()
        .expect("the test ends the fake");
}

/// Empty, truncated and lingering: the job is ended even when the root exits on its own.
#[tokio::test]
async fn a_child_that_exits_or_lingers_is_ended_with_its_job() {
    let base = Base::new("launch-exits");
    let lane = found(&base, "out").await;
    let (start, mut exits) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let control = start.control();
    queue(&lane, start);
    let _pipes = pipes(&mut exits).await;
    until("the exit ends it", || control.is_ended()).await;
    assert_eq!(end(exits).await, Teardown::Done);

    let linger_base = Base::new("launch-lingers");
    let held = linger_base.0.join("held");
    let lane = found(&linger_base, &format!("linger:{}", held.display())).await;
    // The Version's grandchild left this; the question's must make its own.
    fs::remove_file(held.with_extension("ready")).expect("the first grandchild was ready");
    let (start, mut lingers) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let control = start.control();
    queue(&lane, start);
    let _pipes = pipes(&mut lingers).await;
    until("the exit ends it", || control.is_ended()).await;
    assert_eq!(end(lingers).await, Teardown::Done);
    fs::remove_file(&held).expect("the grandchild was ended with the job");
}

/// S7.
#[tokio::test]
async fn a_restarted_lane_sweeps_leftover_launches() {
    let base = Base::new("launch-sweep");
    let file = leftover(&base, "0123456789abcdef0123456789abcdef");
    let mut lane = found(&base, "out").await;
    assert!(launch_dirs(&base).is_empty(), "swept");
    let file_again = leftover(&base, "0123456789abcdef0123456789abcdee");
    let held = locked(&file_again);
    assert!(matches!(version(&mut lane).await, Ok(Outcome::Version(_))));
    assert!(file_again.exists(), "a locked leftover stays");
    drop(held);
    assert!(matches!(version(&mut lane).await, Ok(Outcome::Version(_))));
    assert!(launch_dirs(&base).is_empty());
    assert!(!file.exists());
}

/// Claude Code's session record and MCP log go at the next sweep, and nothing beside them: not
/// the sign-in file at the root of either swept folder's parent, nor its backups.
#[tokio::test]
async fn a_sweep_takes_claude_codes_session_records_and_mcp_logs_and_nothing_beside_them() {
    let base = Base::new("launch-sweep-files");
    let config = base.owned().join("config");
    let cli = base.owned().join("local").join("claude-cli-nodejs");
    let sessions = config.join("sessions");
    let cache = cli.join("Cache");
    let swept = [
        sessions.join("4242.json"),
        cache
            .join("C--work")
            .join("mcp-logs-kumochi")
            .join("1.jsonl"),
    ];
    let kept = [
        config.join(".credentials.json"),
        cli.join(".credentials.json"),
        config.join("backups").join("1.json"),
    ];
    for file in swept.iter().chain(&kept) {
        owned_dirs(&base, file.parent().expect("a folder"));
        fs::write(file, b"left").expect("a file");
    }
    let stamps = |file: &Path| {
        let meta = fs::metadata(file).expect("still there");
        let bytes = fs::read(file).expect("the test's own bytes");
        (
            bytes,
            meta.modified().expect("written"),
            meta.created().expect("made"),
        )
    };
    let before: Vec<_> = kept.iter().map(|file| stamps(file)).collect();
    let _lane = found(&base, "out").await;
    for folder in [&sessions, &cache] {
        assert_eq!(fs::read_dir(folder).expect("kept").count(), 0, "{folder:?}");
    }
    assert_eq!(
        kept.iter().map(|file| stamps(file)).collect::<Vec<_>>(),
        before
    );
}

/// A swept folder that is a junction is refused, so nothing behind it is deleted.
#[tokio::test]
async fn a_swept_folder_that_is_a_junction_is_never_followed() {
    for (name, folder) in [
        ("sessions", ["config", "sessions"].as_slice()),
        ("cache", ["local", "claude-cli-nodejs", "Cache"].as_slice()),
    ] {
        let base = Base::new(&format!("launch-sweep-junction-{name}"));
        let outside = base.0.join("outside");
        // Trusted like Kumochi's own folders, so only the refused junction keeps it.
        owned_dirs(&base, &outside);
        let behind = outside.join("4242.json");
        fs::write(&behind, b"kept").expect("a file outside");
        let link = folder
            .iter()
            .fold(base.owned(), |path, part| path.join(part));
        owned_dirs(&base, &link);
        set_junction(&link, &outside).expect("a junction out of the folder");
        let (host, _switches) = host(&base, "out");
        let mut lane = Lane::start(host).expect("lane");
        assert_eq!(version(&mut lane).await, Err(Failure::Folders), "{name}");
        assert!(lane.close(soon()).await);
        assert_eq!(fs::read(&behind).expect("still there"), b"kept", "{name}");
    }
}

/// S8.
#[tokio::test]
async fn a_locked_file_leaves_the_folder_for_the_sweep() {
    let base = Base::new("launch-folder-left");
    let mut lane = found(&base, "deaf").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (id_tx, id) = std_mpsc::channel();
    start.hooks.id = Some(id_tx);
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    let id = id.recv_timeout(WAIT).expect("the id");
    let file = engine_dir(&base).join(id.hex()).join("work").join("held");
    fs::write(&file, b"").expect("a file in the launch folder");
    let held = locked(&file);
    assert_eq!(end(launch).await, Teardown::FolderLeft);
    assert_eq!(launch_dirs(&base), [id.hex()]);
    drop(held);
    assert!(matches!(version(&mut lane).await, Ok(Outcome::Version(_))));
    assert!(launch_dirs(&base).is_empty());
}

/// S19.
#[tokio::test]
async fn the_subtree_handles_are_released_before_deletion() {
    let base = Base::new("launch-released");
    let lane = found(&base, "deaf").await;
    let (start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    assert_eq!(launch_dirs(&base).len(), 1);
    assert_eq!(end(launch).await, Teardown::Done);
    assert!(launch_dirs(&base).is_empty());
}

/// S20: a start that fails after the listener and the job exist stops them in order.
#[tokio::test]
async fn a_partial_start_stops_in_order() {
    let base = Base::new("launch-partial");
    let lane = found(&base, "deaf").await;
    let mut bad = question(Duration::from_secs(20));
    bad.model = "nul\0inside".to_owned();
    let (mut start, mut launch) = admit(bad).expect("a launch id");
    let (id_tx, id) = std_mpsc::channel();
    start.hooks.id = Some(id_tx);
    queue(&lane, start);
    assert_eq!(failed(&mut launch).await, LaunchFailed::Spawn);
    assert_eq!(end(launch).await, Teardown::Done);
    let id = id.recv_timeout(WAIT).expect("the id");
    assert!(Listener::create(id).is_ok(), "the listener was ended");
    assert!(launch_dirs(&base).is_empty());
    assert_eq!(questions(&base), 0);
}

/// The pipe name: taken while the launch lives, free after, and a squatter fails the launch.
#[tokio::test]
async fn a_launch_owns_its_pipe_name() {
    let base = Base::new("launch-pipe-name");
    let lane = found(&base, "deaf").await;
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (id_tx, id) = std_mpsc::channel();
    start.hooks.id = Some(id_tx);
    queue(&lane, start);
    let _pipes = pipes(&mut launch).await;
    let id = id.recv_timeout(WAIT).expect("the id");
    until("the launch runs", || questions(&base) == 1).await;
    assert!(Listener::create(id).is_err(), "taken while it lives");
    assert_eq!(end(launch).await, Teardown::Done);
    assert!(Listener::create(id).is_ok(), "free after");

    let (mut start, mut squatted) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (id_tx, id) = std_mpsc::channel();
    let (paused, barrier) = barrier();
    start.hooks.id = Some(id_tx);
    start.hooks.folder = Some(paused);
    queue(&lane, start);
    assert!(barrier.reached_within(WAIT), "the folder is made");
    let squatting = Listener::create(id.recv_timeout(WAIT).expect("the id")).expect("squatted");
    barrier.release();
    assert_eq!(failed(&mut squatted).await, LaunchFailed::Listener);
    assert_eq!(end(squatted).await, Teardown::Done);
    drop(squatting);
    assert_eq!(questions(&base), 1);
}

/// S18: a second lane on the same base, as a second brain has, never touches a live launch.
#[tokio::test]
async fn a_second_lane_never_sweeps_a_live_launch() {
    let base = Base::new("launch-two-lanes");
    let lane = found_with(&base, "deaf", |host| host.terminate = slow_terminate).await;
    let (other_host, _switches) = host(&base, "out");
    let mut other = Lane::start(other_host).expect("lane");
    let (mut start, mut launch) = admit(question(Duration::from_secs(20))).expect("a launch id");
    let (paused, barrier) = barrier();
    start.hooks.folder = Some(paused);
    queue(&lane, start);
    assert!(barrier.reached_within(WAIT), "the folder is made");
    // After the launch's own sweep: a leftover nothing holds open, beside the live one.
    let sibling = leftover(&base, "0123456789abcdef0123456789abcdef");
    let mut check = async |when: &str| {
        let before = tree(&base);
        assert_eq!(version(&mut other).await, Err(Failure::Busy), "{when}");
        assert_eq!(tree(&base), before, "{when}");
        assert!(sibling.exists(), "{when}");
    };
    check("before the spawn").await;
    barrier.release();
    let _pipes = pipes(&mut launch).await;
    check("live").await;
    launch.lease().revoke();
    tokio::time::sleep(Duration::from_millis(300)).await;
    check("in the teardown").await;
    assert_eq!(end(launch).await, Teardown::Done);
    let mut lane = lane;
    assert!(lane.close(soon()).await);
    assert!(matches!(version(&mut other).await, Ok(Outcome::Version(_))));
    assert!(!sibling.exists(), "swept once the lock moved");
    assert!(launch_dirs(&base).is_empty());
}

/// Throughput and resources of the stream pumps: 64 MiB from fake `flood` to a round that
/// reads. Run alone on a quiet machine:
/// `cargo test -p kumochi-brain --lib measure_launch_pipes -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "a measurement, not a test"]
async fn measure_launch_pipes() {
    const TOTAL: usize = 64 * 1024 * 1024;
    let base = Base::new("launch-measure");
    let lane = found(&base, "flood").await;
    let (mut start, mut launch) = admit(question(MAX_LAUNCH)).expect("a launch id");
    let (pumps, ended) = std_mpsc::channel();
    start.hooks.pumps = Some(pumps);
    queue(&lane, start);
    let mut pipes = pipes(&mut launch).await;
    let began = Instant::now();
    let (mut read, mut chunks, mut peak) = (0, 0, 0);
    while read < TOTAL {
        peak = peak.max(pipes.stdout.len());
        let chunk = pipes.stdout.recv().await.expect("output");
        read += chunk.len();
        chunks += 1;
    }
    let took = began.elapsed();
    let teardown = end(launch).await;
    let names: Vec<_> = ended.try_iter().collect();
    let mib = f64::from(u32::try_from(read).expect("64 MiB fits")) / (1024.0 * 1024.0);
    let rate = mib / took.as_secs_f64();
    println!(
        "read {read} bytes in {chunks} chunks over {took:?}: {rate:.1} MiB/s; peak queued {peak} chunks (≤ {} bytes); teardown {teardown:?}; pump threads ended: {}",
        peak * CHUNK,
        names.len()
    );
    assert_eq!(teardown, Teardown::Done);
    assert_eq!(names.len(), 3);
}
