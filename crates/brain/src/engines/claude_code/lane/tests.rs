//! The lane against a fake `claude.exe`: this test binary, entered through
//! [`FAKE_CLAUDE`](crate::platform::FAKE_CLAUDE) before libtest's `main`. Every fake start but a
//! held grandchild's appends a line to `starts.log` in its working folder: its arguments, a tab,
//! then its whole environment.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{CLAUDE_CODE_DIR, CLAUDE_CODE_LOCK, OBJECT_PREFIX};
use kumochi_protocol::message::{ClaudeVersion, PolicyFailed};

use super::{
    DEADLINE, Done, Failure, Host, LANE_QUEUE, Lane, Outcome, Program, STATUS_CAP, STDERR_CAP,
    VERSION_CAP, Work,
};
use crate::engines::claude_code::auth::StatusFailure;
use crate::engines::claude_code::install::FindError;
use crate::platform::{FAKE_CLAUDE, Job, ManagedPresence, Presence, SessionLock, local_app_data};

/// The version every fake reports.
pub(crate) const V: ClaudeVersion = ClaudeVersion {
    major: 2,
    minor: 1,
    patch: 306,
};

/// The fake, from `platform::FAKE_CLAUDE`: `args` is its mode, then the entry point's arguments.
///
/// Modes: `out` and `in` (signed out or in), `hang`, `version:N` (`--version` prints exactly N
/// bytes), `status:N` (the status JSON padded to N bytes), `stderr:N` (N bytes of stderr first),
/// `exit:N` (status exits N) and `linger:PATH` (leaves a grandchild holding PATH and stdout).
pub(crate) fn fake(args: Vec<OsString>) -> i32 {
    let mut args = args
        .into_iter()
        .map(|arg| arg.into_string().expect("the tests pass text"));
    let mode = args.next().expect("a mode");
    let rest: Vec<String> = args.collect();
    if let Some(path) = mode.strip_prefix("hold:") {
        hold(Path::new(path));
        return 0;
    }
    record(&rest);
    let (kind, value) = mode.split_once(':').unwrap_or((mode.as_str(), ""));
    let size = || value.parse::<usize>().expect("a size");
    match kind {
        "hang" => {
            std::thread::sleep(Duration::from_secs(60));
            return 0;
        }
        "stderr" => {
            let mut stderr = std::io::stderr();
            stderr.write_all(&vec![b'e'; size()]).expect("stderr");
            stderr.flush().expect("stderr");
        }
        "linger" => linger(Path::new(value)),
        _ => {}
    }
    let mut stdout = std::io::stdout();
    if rest.first().is_none_or(|word| word != "auth") {
        let line = match kind {
            "version" if size() == 0 => String::new(),
            "version" => format!("2.1.306{}", " ".repeat(size() - 7)),
            _ => "2.1.306 (Claude Code)\n".to_owned(),
        };
        stdout.write_all(line.as_bytes()).expect("stdout");
        stdout.flush().expect("stdout");
        return 0;
    }
    let signed_in = kind == "in";
    let config = std::env::var("CLAUDE_CONFIG_DIR").expect("the lane sets it");
    let mut json = format!(
        r#"{{"loggedIn":{signed_in},"authMethod":"none","configDirectory":{}}}"#,
        serde_json::to_string(&config).expect("a string")
    );
    if kind == "status" {
        json.push_str(&" ".repeat(size() - json.len()));
    }
    stdout.write_all(json.as_bytes()).expect("stdout");
    stdout.flush().expect("stdout");
    match kind {
        "exit" => size().try_into().expect("small"),
        _ if signed_in => 0,
        _ => 1,
    }
}

fn record(args: &[String]) {
    let env: Vec<String> = std::env::vars_os()
        .map(|(name, value)| format!("{}={}", name.to_string_lossy(), value.to_string_lossy()))
        .collect();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open("starts.log")
        .expect("the working folder is writable");
    writeln!(log, "{}\t{}", args.join(" "), env.join("\u{1f}")).expect("log");
}

/// Starts a grandchild that holds `path` with no sharing, and the root's stdout, and returns
/// once it holds them.
#[expect(
    clippy::disallowed_methods,
    clippy::zombie_processes,
    reason = "the grandchild must outlive the fake, holding the stdout it inherits"
)]
fn linger(path: &Path) {
    let mut hold: OsString = "hold:".into();
    hold.push(path);
    std::process::Command::new(std::env::current_exe().expect("exe"))
        .args([OsString::from(FAKE_CLAUDE), hold])
        .spawn()
        .expect("grandchild");
    let ready = path.with_extension("ready");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the grandchild got ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn hold(path: &Path) {
    let _held = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .share_mode(0)
        .open(path)
        .expect("hold");
    fs::write(path.with_extension("ready"), b"").expect("ready");
    std::thread::sleep(Duration::from_secs(60));
}

/// A fresh trusted folder for one test's lane, removed on drop. The temp folder won't do: on Rez
/// other accounts may write to it, which the folder checks refuse.
pub(crate) struct Base(pub(crate) PathBuf);

impl Base {
    pub(crate) fn new(name: &str) -> Self {
        let dir = local_app_data().expect("local app data").join(format!(
            "{OBJECT_PREFIX}-test-{}-claude-{name}",
            std::process::id()
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("old scratch folder removable");
        }
        fs::create_dir(&dir).expect("scratch folder");
        Self(dir)
    }

    pub(crate) fn owned(&self) -> PathBuf {
        self.0.join(CLAUDE_CODE_DIR)
    }

    /// Every recorded start, oldest first.
    pub(crate) fn starts(&self) -> Vec<String> {
        fs::read_to_string(self.owned().join("work").join("starts.log"))
            .map(|log| log.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Each recorded start's arguments.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.starts()
            .iter()
            .map(|start| start.split('\t').next().expect("args").to_owned())
            .collect()
    }

    fn lock(&self) -> PathBuf {
        self.0.join(CLAUDE_CODE_LOCK)
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// What a test flips under a running lane.
pub(crate) struct Switches {
    pub(crate) vault_held: Arc<AtomicBool>,
    pub(crate) managed: Arc<AtomicBool>,
}

/// A host whose install is the fake in `mode`, with the vault held and no managed policy.
pub(crate) fn host(base: &Base, mode: &str) -> (Host, Switches) {
    let mode = mode.to_owned();
    host_finding(
        base,
        Box::new(move || {
            Ok(Program {
                exe: std::env::current_exe().map_err(|_| FindError::NotFound)?,
                prefix: vec![FAKE_CLAUDE.into(), mode.clone().into()],
            })
        }),
    )
}

pub(crate) fn host_finding(
    base: &Base,
    find: Box<dyn Fn() -> Result<Program, FindError> + Send>,
) -> (Host, Switches) {
    let switches = Switches {
        vault_held: Arc::new(AtomicBool::new(true)),
        managed: Arc::new(AtomicBool::new(false)),
    };
    let managed = Arc::clone(&switches.managed);
    let host = Host {
        find,
        base: base.0.clone(),
        vault_held: Arc::clone(&switches.vault_held),
        managed: Box::new(move || ManagedPresence {
            files: [Presence::Absent; 3],
            machine: Presence::Absent,
            user: if managed.load(Ordering::SeqCst) {
                Presence::Present
            } else {
                Presence::Absent
            },
        }),
        brain_job: Arc::new(Job::new().expect("brain job")),
    };
    (host, switches)
}

pub(crate) fn soon() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(5)
}

async fn next(lane: &mut Lane) -> Done {
    tokio::time::timeout(Duration::from_secs(30), lane.next())
        .await
        .expect("the lane answers")
}

/// Runs `work` at generation 0, which every new lane admits.
async fn call(lane: &mut Lane, work: Work) -> Result<Outcome, Failure> {
    assert!(lane.submit(0, work), "admitted");
    let done = next(lane).await;
    assert_eq!(done.generation, 0);
    done.result
}

type Both = (
    Result<Outcome, Failure>,
    Result<Outcome, Failure>,
    Vec<String>,
);

/// Version then Status from a lane on the fake in `mode`, and each start's arguments.
async fn both(mode: &str) -> Both {
    let base = Base::new(&mode.replace(':', "-"));
    let (host, _switches) = host(&base, mode);
    let mut lane = Lane::start(host).expect("lane");
    let version = call(&mut lane, Work::Version).await;
    let status = call(&mut lane, Work::Status).await;
    assert!(lane.close(soon()).await, "closed clean");
    (version, status, base.calls())
}

const SIGNED_OUT: Outcome = Outcome::Status {
    version: V,
    signed_in: false,
};

#[tokio::test]
async fn version_then_status_reads_the_fakes_answers() {
    let calls = vec!["--version".to_owned(), "auth status --json".to_owned()];
    assert_eq!(
        both("out").await,
        (Ok(Outcome::Version(V)), Ok(SIGNED_OUT), calls.clone())
    );
    assert_eq!(
        both("in").await,
        (
            Ok(Outcome::Version(V)),
            Ok(Outcome::Status {
                version: V,
                signed_in: true
            }),
            calls
        )
    );
}

#[tokio::test]
async fn the_child_sees_only_kumochis_folders_and_the_system_environment() {
    let planted = "CARGO_MANIFEST_DIR";
    assert!(
        std::env::var_os(planted).is_some(),
        "cargo gives the test process a variable to look for"
    );
    let real_profile = PathBuf::from(std::env::var_os("USERPROFILE").expect("a profile"));
    let base = Base::new("isolation");
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert!(lane.close(soon()).await);
    let starts = base.starts();
    assert_eq!(starts.len(), 1, "the log is in the owned work folder");
    let (args, env) = starts[0].split_once('\t').expect("args and env");
    assert_eq!(args, "--version");
    let env: Vec<(&str, &str)> = env
        .split('\u{1f}')
        .map(|pair| pair.split_once('=').expect("name=value"))
        .collect();
    let value = |name: &str| {
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| PathBuf::from(value))
    };
    assert_eq!(value("USERPROFILE"), Some(base.owned().join("profile")));
    assert_eq!(
        value("CLAUDE_CONFIG_DIR"),
        Some(base.owned().join("config"))
    );
    assert_eq!(value(planted), None, "nothing is inherited");
    for (name, value) in &env {
        assert_ne!(
            Path::new(value),
            real_profile,
            "{name} names the real profile"
        );
    }
}

#[tokio::test]
async fn managed_policy_before_version_starts_nothing() {
    let base = Base::new("managed-first");
    let (host, switches) = host(&base, "out");
    switches.managed.store(true, Ordering::SeqCst);
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Err(Failure::Policy(PolicyFailed::ManagedPolicy))
    );
    assert_eq!(
        call(&mut lane, Work::Status).await,
        Err(Failure::Find(FindError::NotFound)),
        "no status without a version found"
    );
    assert!(lane.close(soon()).await);
    assert_eq!(base.starts(), Vec::<String>::new());
}

#[tokio::test]
async fn managed_policy_appearing_before_status_starts_nothing_more() {
    let base = Base::new("managed-later");
    let (host, switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    switches.managed.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&mut lane, Work::Status).await,
        Err(Failure::Policy(PolicyFailed::ManagedPolicy))
    );
    assert!(lane.close(soon()).await);
    assert_eq!(base.calls(), ["--version"]);
}

#[tokio::test]
async fn without_the_vault_or_with_the_claude_lock_busy_nothing_starts() {
    let base = Base::new("guard");
    let (without_vault, switches) = host(&base, "out");
    switches.vault_held.store(false, Ordering::SeqCst);
    let mut lane = Lane::start(without_vault).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Refused));
    assert!(!base.owned().exists(), "no folder was made");
    assert!(
        SessionLock::try_acquire(&base.lock()).is_ok(),
        "the lane didn't take the claude lock"
    );
    assert!(lane.close(soon()).await);

    let (host, _switches) = host(&base, "out");
    let other = SessionLock::try_acquire(&base.lock()).expect("another brain's lock");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Refused));
    assert!(lane.close(soon()).await);
    drop(other);
    assert!(!base.owned().exists(), "no folder was made");
}

#[tokio::test]
async fn a_stale_queued_job_does_nothing() {
    let base = Base::new("fence");
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    lane.fence(2);
    assert!(
        lane.submit(0, Work::Status),
        "queued under the old generation"
    );
    assert!(lane.submit(2, Work::Version));
    let done = next(&mut lane).await;
    assert_eq!(
        (done.generation, done.result),
        (2, Ok(Outcome::Version(V))),
        "the stale Status published nothing"
    );
    assert!(lane.close(soon()).await);
    assert_eq!(base.calls(), ["--version", "--version"]);
}

#[tokio::test]
async fn close_ends_a_running_call_and_only_then_lets_the_lock_go() {
    let base = Base::new("close");
    let (host, _switches) = host(&base, "hang");
    let mut lane = Lane::start(host).expect("lane");
    assert!(lane.submit(0, Work::Version));
    let started = Instant::now();
    while base.starts().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the fake started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        SessionLock::try_acquire(&base.lock()).is_err(),
        "held while the call runs"
    );
    let closing = Instant::now();
    assert!(lane.close(soon()).await, "every process confirmed ended");
    assert!(
        closing.elapsed() < Duration::from_secs(3),
        "close didn't wait for the call's deadline"
    );
    assert!(SessionLock::try_acquire(&base.lock()).is_ok(), "let go");
}

#[tokio::test]
async fn a_call_past_its_deadline_fails() {
    let base = Base::new("deadline");
    let (host, _switches) = host(&base, "hang");
    let mut lane = Lane::start(host).expect("lane");
    let started = Instant::now();
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Deadline));
    let took = started.elapsed();
    assert!(took >= DEADLINE && took < DEADLINE * 2, "took {took:?}");
    assert!(lane.close(soon()).await);
}

#[tokio::test]
async fn a_grandchild_holding_stdout_is_ended_before_the_result() {
    let base = Base::new("linger");
    let held = base.0.join("held");
    let (host, _switches) = host(&base, &format!("linger:{}", held.display()));
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    OpenOptions::new()
        .write(true)
        .share_mode(0)
        .open(&held)
        .expect("the grandchild had ended when the result came");
    assert_eq!(call(&mut lane, Work::Status).await, Ok(SIGNED_OUT));
    assert!(lane.close(soon()).await);
    assert_eq!(base.calls(), ["--version", "auth status --json"]);
}

#[tokio::test]
async fn each_pipe_is_capped_at_its_exact_bound() {
    let version = |n: usize| format!("version:{n}");
    assert_eq!(both(&version(VERSION_CAP)).await.0, Ok(Outcome::Version(V)));
    assert_eq!(
        both(&version(VERSION_CAP + 1)).await.0,
        Err(Failure::TooLong)
    );
    let status = |n: usize| format!("status:{n}");
    assert_eq!(both(&status(STATUS_CAP)).await.1, Ok(SIGNED_OUT));
    assert_eq!(both(&status(STATUS_CAP + 1)).await.1, Err(Failure::TooLong));
    let stderr = |n: usize| format!("stderr:{n}");
    assert_eq!(both(&stderr(STDERR_CAP)).await.0, Ok(Outcome::Version(V)));
    assert_eq!(both(&stderr(STDERR_CAP + 1)).await.0, Err(Failure::TooLong));
}

#[tokio::test]
async fn empty_or_failed_output_fails_the_call() {
    assert_eq!(both("version:0").await.0, Err(Failure::Version));
    assert_eq!(
        both("exit:2").await.1,
        Err(Failure::Status(StatusFailure::Exit))
    );
}

#[tokio::test]
async fn a_full_queue_refuses_without_waiting() {
    let base = Base::new("queue");
    let (entered_tx, entered) = std_mpsc::channel();
    let (release, released) = std_mpsc::channel::<()>();
    let released = Mutex::new(released);
    let (host, _switches) = host_finding(
        &base,
        Box::new(move || {
            entered_tx.send(()).ok();
            released.lock().expect("gate").recv().ok();
            Err(FindError::NotFound)
        }),
    );
    let mut lane = Lane::start(host).expect("lane");
    assert!(lane.submit(0, Work::Version));
    tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the first job is running");
    })
    .await
    .expect("joined");
    let started = Instant::now();
    for _ in 0..LANE_QUEUE {
        assert!(lane.submit(0, Work::Version), "room in the queue");
    }
    assert!(!lane.submit(0, Work::Version), "full");
    assert!(started.elapsed() < Duration::from_secs(1), "never waited");
    drop(release);
    for _ in 0..=LANE_QUEUE {
        assert_eq!(
            next(&mut lane).await.result,
            Err(Failure::Find(FindError::NotFound))
        );
    }
    assert!(lane.close(soon()).await);
}
