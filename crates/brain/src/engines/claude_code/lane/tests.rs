//! The lane against a fake `claude.exe`: this test binary, entered through
//! [`FAKE_CLAUDE`](crate::platform::FAKE_CLAUDE) before libtest's `main`. Every fake start but a
//! held grandchild's appends a line to `starts.log` in the test's base folder, two above its
//! working folder, so the log outlives a Disconnect: its arguments, a tab, then its whole
//! environment.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write as _};
use std::os::windows::fs::{OpenOptionsExt as _, symlink_file};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use kumochi_protocol::constants::{
    CLAUDE_CODE_DELETING, CLAUDE_CODE_DIR, CLAUDE_CODE_LOCK, OBJECT_PREFIX,
};
use kumochi_protocol::message::{ClaudeVersion, PolicyFailed};
use windows_sys::Win32::Security::{CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

use super::{
    DEADLINE, Done, Failure, Host, LANE_QUEUE, Lane, Outcome, Program, STATUS_CAP, STDERR_CAP,
    VERSION_CAP, Work, drain, output,
};
use crate::engines::claude_code::auth::StatusFailure;
use crate::engines::claude_code::install::FindError;
use crate::platform::Stdio;
use crate::platform::sherpa::tests::{icacls, set_junction};
use crate::platform::{
    DaclEntry, FAKE_CLAUDE, Job, ManagedPresence, Presence, SessionLock, dacl_entries, dacl_sddl,
    local_app_data, pause_after_check, set_dacl, this_user,
};

/// The version every fake reports.
pub(crate) const V: ClaudeVersion = ClaudeVersion {
    major: 2,
    minor: 1,
    patch: 306,
};

/// The fake, from `platform::FAKE_CLAUDE`: `args` is its mode, then the entry point's arguments.
///
/// Modes: `out` and `in` (signed out or in), `hang`, `version:N` (`--version` prints exactly N
/// bytes), `other` and `older` (`--version` prints an unreviewed 2.1.307 or 2.1.305), `status:N` (the status JSON padded to N bytes), `stderr:N` (N bytes of stderr first),
/// `exit:N` (status exits N), `linger:PATH` (leaves a grandchild holding PATH and stdout),
/// `loginhang` (`auth login` never ends) and `logoutfail` (`auth logout` exits 3). A question
/// (`-p`) runs as [`question`] says. Whatever the
/// mode, `auth login` leaves a sign-in in the config folder, which `auth status` then reports and
/// `auth logout` removes, noting whether the Disconnect marker was down.
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
    if rest.first().is_some_and(|word| word == "-p") {
        return question(kind);
    }
    let mut stdout = std::io::stdout();
    if rest.first().is_none_or(|word| word != "auth") {
        let line = match kind {
            "version" if size() == 0 => String::new(),
            "version" => format!("2.1.306{}", " ".repeat(size() - 7)),
            "other" => "2.1.307 (Claude Code)\n".to_owned(),
            "older" => "2.1.305 (Claude Code)\n".to_owned(),
            _ => "2.1.306 (Claude Code)\n".to_owned(),
        };
        stdout.write_all(line.as_bytes()).expect("stdout");
        stdout.flush().expect("stdout");
        return 0;
    }
    let config = std::env::var("CLAUDE_CONFIG_DIR").expect("the lane sets it");
    let token = Path::new(&config).join("signed-in");
    match rest.get(1).map(String::as_str) {
        Some("login") => {
            if kind == "loginhang" {
                std::thread::sleep(Duration::from_secs(60));
            }
            fs::write(&token, b"").expect("sign in");
            return 0;
        }
        Some("logout") => {
            if Path::new("../..").join(CLAUDE_CODE_DELETING).exists() {
                fs::write("../../logout-saw-marker", b"").expect("note");
            }
            fs::remove_file(&token).ok();
            return if kind == "logoutfail" { 3 } else { 0 };
        }
        _ => {}
    }
    let signed_in = kind == "in" || token.exists();
    let method = if signed_in { "claude.ai" } else { "none" };
    let mut json = format!(
        r#"{{"loggedIn":{signed_in},"authMethod":"{method}","apiProvider":"firstParty","configDirectory":{}}}"#,
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

/// The test's base folder, two above `CLAUDE_CONFIG_DIR`, wherever the start's working folder is.
fn base_dir() -> PathBuf {
    let config = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").expect("the lane sets it"));
    config
        .ancestors()
        .nth(2)
        .expect("the config folder is two below the base")
        .to_owned()
}

/// A question: `echo` copies stdin to stdout and logs each read's length to `echo.log`,
/// `deaf` never reads stdin, `flood` writes stdout until it can't, and any other mode exits.
fn question(kind: &str) -> i32 {
    match kind {
        "echo" => {
            let mut buffer = vec![0; 64 * 1024];
            let mut stdin = std::io::stdin();
            let mut stdout = std::io::stdout();
            loop {
                let read = stdin.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    return 0;
                }
                let mut log = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(base_dir().join("echo.log"))
                    .expect("the base folder is writable");
                writeln!(log, "{read}").expect("log");
                if stdout
                    .write_all(&buffer[..read])
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return 0;
                }
            }
        }
        "deaf" => std::thread::sleep(Duration::from_secs(60)),
        "flood" => {
            let chunk = vec![b'x'; 64 * 1024];
            let mut stdout = std::io::stdout();
            while stdout.write_all(&chunk).is_ok() {}
        }
        _ => {}
    }
    0
}

fn record(args: &[String]) {
    let env: Vec<String> = std::env::vars_os()
        .map(|(name, value)| format!("{}={}", name.to_string_lossy(), value.to_string_lossy()))
        .collect();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(base_dir().join("starts.log"))
        .expect("the base folder is writable");
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
        fs::read_to_string(self.0.join("starts.log"))
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

    pub(crate) fn marker(&self) -> PathBuf {
        self.0.join(CLAUDE_CODE_DELETING)
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
        console: Stdio::Null,
        terminate: Job::terminate,
        brain_exe: std::env::current_exe().expect("the test binary"),
        runtime: tokio::runtime::Handle::current(),
    };
    (host, switches)
}

pub(crate) fn soon() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(5)
}

/// Waits until the fake has recorded `args`.
pub(crate) async fn started(base: &Base, args: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !base.calls().iter().any(|call| call == args) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the call started");
}

/// A Disconnect's marker and a file in Kumochi's folder, as a brain stopped part way leaves them;
/// the file's path.
pub(crate) fn marked(base: &Base) -> PathBuf {
    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folder");
    let left = config.join("left.json");
    fs::write(&left, b"{}").expect("a file in it");
    fs::write(base.marker(), b"").expect("the marker");
    left
}

/// `path` open with no sharing, so it can't be deleted while this lives.
pub(crate) fn locked(path: &Path) -> fs::File {
    OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .expect("held")
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
    assert_eq!(starts.len(), 1, "one start");
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
    // Kumochi's own folders are the only paths under the real profile a child may see.
    for (name, value) in &env {
        for path in value.split(';').map(Path::new) {
            assert!(
                !path.starts_with(&real_profile) || path.starts_with(&base.0),
                "{name} names a folder in the real profile"
            );
        }
    }
}

/// A pipe that gives each step in turn, then its end.
struct Script(VecDeque<io::Result<Vec<u8>>>);

impl Script {
    fn new(steps: impl IntoIterator<Item = io::Result<Vec<u8>>>) -> Self {
        Self(steps.into_iter().collect())
    }
}

impl Read for Script {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.pop_front() {
            None => Ok(0),
            Some(Err(error)) => Err(error),
            Some(Ok(bytes)) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }
    }
}

const ANSWER: &[u8] = b"2.1.306 (Claude Code)\n";

fn drained(pipe: Script) -> Option<super::Drained> {
    drain(pipe, VERSION_CAP)
        .recv_timeout(Duration::from_secs(5))
        .ok()
}

fn whole() -> Script {
    Script::new([Ok(ANSWER.to_vec())])
}

fn broken() -> Script {
    Script::new([Ok(ANSWER.to_vec()), Err(io::ErrorKind::Other.into())])
}

#[test]
fn a_pipe_that_fails_after_a_whole_answer_fails_the_call() {
    assert_eq!(
        output(drained(broken()), drained(Script::new([]))),
        Err(Failure::Pipe),
        "stdout"
    );
    assert_eq!(
        output(drained(whole()), drained(broken())),
        Err(Failure::Pipe),
        "stderr"
    );
}

#[test]
fn an_interrupted_read_is_retried_to_the_end() {
    let interrupted = Script::new([
        Ok(ANSWER[..4].to_vec()),
        Err(io::ErrorKind::Interrupted.into()),
        Ok(ANSWER[4..].to_vec()),
    ]);
    assert_eq!(
        output(drained(interrupted), drained(Script::new([]))),
        Ok(ANSWER.to_vec())
    );
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
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Busy));
    assert!(!base.owned().exists(), "no folder was made");
    assert!(
        SessionLock::try_acquire(&base.lock()).is_ok(),
        "the lane didn't take the claude lock"
    );
    assert!(lane.close(soon()).await);

    let (host, _switches) = host(&base, "out");
    let other = SessionLock::try_acquire(&base.lock()).expect("another brain's lock");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Busy));
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

#[tokio::test]
async fn a_marked_folder_is_deleted_before_anything_starts() {
    let base = Base::new("marked");
    let left = marked(&base);
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert!(lane.close(soon()).await);
    assert!(!left.exists(), "the old folder went first");
    assert!(!base.marker().exists());
    assert_eq!(base.calls(), ["--version"]);
}

#[tokio::test]
async fn while_the_marked_folder_cant_be_deleted_nothing_starts() {
    let base = Base::new("marked-locked");
    let left = marked(&base);
    let held = locked(&left);
    let (host, _switches) = host(&base, "in");
    let mut lane = Lane::start(host).expect("lane");
    for work in [Work::Version, Work::Status, Work::Login] {
        assert_eq!(
            call(&mut lane, work).await,
            Err(Failure::Cleanup),
            "{work:?}"
        );
    }
    assert_eq!(base.calls(), Vec::<String>::new());
    assert!(
        base.marker().exists(),
        "the marker stays until the folder is gone"
    );
    drop(held);
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert!(lane.close(soon()).await);
    assert!(!left.exists());
    assert!(!base.marker().exists());
}

#[tokio::test]
async fn a_reparse_point_in_the_folder_is_refused_and_its_target_kept() {
    let base = Base::new("marked-reparse");
    marked(&base);
    let outside = base.0.join("outside");
    fs::create_dir(&outside).expect("outside");
    let kept = outside.join("kept.txt");
    fs::write(&kept, b"kept").expect("a file outside");
    let link = base.owned().join("config").join("link");
    fs::create_dir(&link).expect("an empty folder");
    set_junction(&link, &outside).expect("a junction out of the folder");
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Cleanup));
    assert!(lane.close(soon()).await);
    assert_eq!(fs::read(&kept).expect("still there"), b"kept");
    assert!(link.exists(), "refused, not removed");
    assert!(base.marker().exists());
    assert_eq!(base.calls(), Vec::<String>::new());
}

/// What a scratch file outside Kumochi's folder holds.
const KEPT: &[u8] = b"kept, not Kumochi's";

/// A scratch file outside Kumochi's folder holding [`KEPT`].
fn kept(base: &Base) -> PathBuf {
    let outside = base.0.join("outside");
    fs::create_dir_all(&outside).expect("outside");
    let kept = outside.join("kept.txt");
    fs::write(&kept, KEPT).expect("a file outside");
    kept
}

/// Renames the folder `dir` aside and puts a junction to `target` in its place.
fn swap_for_junction(dir: &Path, target: &Path) -> io::Result<()> {
    fs::rename(dir, dir.with_extension("moved"))?;
    fs::create_dir(dir)?;
    set_junction(dir, target)
}

#[tokio::test]
async fn a_folder_swapped_for_a_junction_once_checked_is_never_followed() {
    let base = Base::new("swapped");
    marked(&base);
    let config = base.owned().join("config");
    fs::create_dir(config.join("inner")).expect("a folder in it");
    let kept = kept(&base);
    let outside = kept.parent().expect("outside").to_owned();
    let renamed = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&renamed);
    let checked = config.clone();
    pause_after_check(
        config,
        Box::new(move || {
            noted.store(
                swap_for_junction(&checked, &outside).is_ok(),
                Ordering::SeqCst,
            );
            // The checked folder is held, but what's in it isn't checked yet.
            swap_for_junction(&checked.join("inner"), &outside).ok();
        }),
    );
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Cleanup));
    assert!(lane.close(soon()).await);
    assert_eq!(fs::read(&kept).expect("still there"), KEPT);
    assert!(
        !renamed.load(Ordering::SeqCst),
        "the checked folder kept its name"
    );
    assert!(base.marker().exists(), "the deletion waits");
    assert_eq!(base.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_nested_folder_others_can_change_stops_the_deletion_before_its_listed() {
    let base = Base::new("writable");
    marked(&base);
    let deep = base.owned().join("config").join("deep");
    fs::create_dir(&deep).expect("a nested folder");
    fs::write(deep.join("left.json"), b"{}").expect("a file in it");
    icacls(&deep, &["/grant", "*S-1-1-0:(M)"]);
    let kept = kept(&base);
    let outside = kept.parent().expect("outside").to_owned();
    let checked = deep.clone();
    pause_after_check(
        deep.clone(),
        Box::new(move || {
            swap_for_junction(&checked, &outside).ok();
        }),
    );
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Cleanup));
    assert!(lane.close(soon()).await);
    assert_eq!(fs::read(&kept).expect("still there"), KEPT);
    assert!(deep.join("left.json").exists(), "refused, not emptied");
    assert!(base.marker().exists(), "the deletion waits");
    assert_eq!(base.calls(), Vec::<String>::new());
}

/// Kumochi's folder and its `config` folder made before the lane runs, the folder readable by this
/// user and SYSTEM only and `config` with this user's full control plus `extra` ACEs; then a
/// Version's result.
async fn version_with_config(base: &Base, extra: &str) -> Result<Outcome, Failure> {
    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folders");
    set_dacl(&base.owned(), |user| {
        format!("D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)")
    });
    set_dacl(&config, |user| format!("D:P(A;OICI;FA;;;{user}){extra}"));
    let (host, _switches) = host(base, "out");
    let mut lane = Lane::start(host).expect("lane");
    let version = call(&mut lane, Work::Version).await;
    assert!(lane.close(soon()).await);
    version
}

#[tokio::test]
async fn a_reused_config_folder_everyone_may_read_is_refused() {
    let base = Base::new("config-readable");
    assert_eq!(
        version_with_config(&base, "(A;OICI;0x1200a9;;;WD)").await,
        Err(Failure::Folders)
    );
    assert_eq!(base.starts(), Vec::<String>::new());
}

#[tokio::test]
async fn a_reused_config_folder_whose_new_files_another_account_may_read_is_refused() {
    let base = Base::new("config-inherits-readers");
    assert_eq!(
        version_with_config(&base, "(A;OICIIO;GR;;;S-1-5-21-1-2-3-1002)").await,
        Err(Failure::Folders)
    );
    assert_eq!(base.starts(), Vec::<String>::new());
}

#[tokio::test]
async fn a_retained_file_another_account_may_read_is_refused() {
    let base = Base::new("config-file-readable");
    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folders");
    let kept = config.join("kept.json");
    fs::write(&kept, b"{}").expect("a file left in it");
    set_dacl(&kept, |user| format!("D:P(A;;FA;;;{user})(A;;FR;;;WD)"));
    assert_eq!(
        version_with_config(&base, "(A;OICI;FA;;;SY)").await,
        Err(Failure::Folders)
    );
    assert_eq!(fs::read(&kept).expect("left alone"), b"{}");
    assert_eq!(base.starts(), Vec::<String>::new());
}

/// Asserts a config folder whose DACL adds `extra`, a grant of no read right, is refused and left
/// exactly as it was.
async fn assert_config_refused_unchanged(name: &str, extra: &str) {
    let base = Base::new(name);
    let config = base.owned().join("config");
    let before = {
        fs::create_dir_all(&config).expect("the folders");
        set_dacl(&config, |user| format!("D:P(A;OICI;FA;;;{user}){extra}"));
        dacl_sddl(&config)
    };
    assert_eq!(
        version_with_config(&base, extra).await,
        Err(Failure::Folders)
    );
    assert_eq!(dacl_sddl(&config), before, "not re-secured");
    assert_eq!(base.starts(), Vec::<String>::new());
}

#[tokio::test]
async fn a_reused_config_folder_whose_new_files_another_account_may_re_acl_is_refused() {
    assert_config_refused_unchanged(
        "config-inherits-write-dac",
        "(A;OICIIO;0x00040000;;;S-1-5-21-1-2-3-1002)",
    )
    .await;
}

#[tokio::test]
async fn a_reused_config_folder_whose_new_files_another_account_may_take_is_refused() {
    assert_config_refused_unchanged(
        "config-inherits-write-owner",
        "(A;OICIIO;0x00080000;;;S-1-5-21-1-2-3-1002)",
    )
    .await;
}

#[tokio::test]
async fn a_retained_file_another_account_may_re_acl_is_refused() {
    let base = Base::new("config-file-write-dac");
    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folders");
    let kept = config.join("kept.json");
    fs::write(&kept, b"{}").expect("a file left in it");
    set_dacl(&kept, |user| {
        format!("D:P(A;;FA;;;{user})(A;;0x00040000;;;S-1-5-21-1-2-3-1002)")
    });
    let before = dacl_sddl(&kept);
    assert_eq!(
        version_with_config(&base, "(A;OICI;FA;;;SY)").await,
        Err(Failure::Folders)
    );
    assert_eq!(dacl_sddl(&kept), before, "not re-secured");
    assert_eq!(base.starts(), Vec::<String>::new());
}

#[tokio::test]
async fn a_retained_file_another_account_may_write_is_refused() {
    let base = Base::new("config-file-data");
    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folders");
    let kept = config.join("kept.json");
    fs::write(&kept, b"{}").expect("a file left in it");
    set_dacl(&kept, |user| {
        format!("D:P(A;;FA;;;{user})(A;;0x00000002;;;S-1-5-21-1-2-3-1002)")
    });
    let before = dacl_sddl(&kept);
    assert_eq!(
        version_with_config(&base, "(A;OICI;FA;;;SY)").await,
        Err(Failure::Folders)
    );
    assert_eq!(dacl_sddl(&kept), before, "not re-secured");
    assert_eq!(base.starts(), Vec::<String>::new());
}

#[tokio::test]
async fn a_sign_in_the_fake_writes_gets_only_this_users_access() {
    let base = Base::new("sign-in-private");
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert_eq!(call(&mut lane, Work::Login).await, Ok(Outcome::LoginEnded));
    assert!(lane.close(soon()).await);
    let config = base.owned().join("config");
    assert_eq!(
        entries(&config),
        [(
            true,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            FILE_ALL_ACCESS,
            this_user()
        )],
        "its folder passes down only this user's access"
    );
    assert_eq!(
        entries(&config.join("signed-in")),
        [(true, 0, FILE_ALL_ACCESS, this_user())]
    );
}

/// `path`'s DACL as each entry's allow, flags, rights and SID. Whether Windows marks an entry
/// inherited varies by account, so that's left out.
fn entries(path: &Path) -> Vec<(bool, u32, u32, String)> {
    dacl_entries(path)
        .into_iter()
        .map(
            |DaclEntry {
                 allow,
                 flags,
                 rights,
                 sid,
                 ..
             }| (allow, flags, rights, sid),
        )
        .collect()
}

#[tokio::test]
async fn a_plain_file_others_can_change_stops_the_deletion() {
    let base = Base::new("marked-file-writable");
    let left = marked(&base);
    icacls(&left, &["/grant", "*S-1-1-0:(M)"]);
    let (host, _switches) = host(&base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Cleanup));
    assert!(lane.close(soon()).await);
    assert_eq!(fs::read(&left).expect("refused, not deleted"), b"{}");
    assert!(base.marker().exists(), "the deletion waits");
    assert_eq!(base.calls(), Vec::<String>::new());
}

/// Asserts a fresh lane's Version and Cleanup both refuse the deletion an invalid marker at
/// `base` would ask for, leaving `left` in Kumochi's folder and starting nothing.
async fn assert_no_recovery(base: &Base, left: &Path) {
    let starts = base.starts();
    let (host, _switches) = host(base, "out");
    let mut lane = Lane::start(host).expect("lane");
    for work in [Work::Version, Work::Cleanup] {
        assert_eq!(
            call(&mut lane, work).await,
            Err(Failure::Cleanup),
            "{work:?}"
        );
    }
    assert!(lane.close(soon()).await);
    assert!(left.exists(), "nothing deleted");
    assert_eq!(base.starts(), starts, "nothing started");
}

#[tokio::test]
async fn an_invalid_marker_found_at_startup_deletes_nothing() {
    let base = Base::new("marker-invalid-at-start");
    let config = base.owned().join("config");
    fs::create_dir_all(&config).expect("the folders");
    let left = config.join("left.json");
    fs::write(&left, b"{}").expect("a file in it");
    let kept = kept(&base);
    fs::hard_link(&kept, base.marker()).expect("a hard link at the marker");
    assert_no_recovery(&base, &left).await;
    assert_eq!(fs::read(&kept).expect("still there"), KEPT);
}

#[tokio::test]
async fn a_reused_config_folder_only_this_user_and_system_may_read_is_used() {
    let base = Base::new("config-private");
    assert_eq!(
        version_with_config(&base, "(A;OICI;FA;;;SY)").await,
        Ok(Outcome::Version(V))
    );
    assert_eq!(base.calls(), ["--version"]);
}

/// Signs in on the fake, runs `plant`, then asserts a Disconnect refuses: no sign-out, nothing
/// deleted and `kept` still holding [`KEPT`].
async fn assert_disconnect_refused(base: &Base, kept: &Path, plant: impl FnOnce()) {
    let (host, _switches) = host(base, "out");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert_eq!(call(&mut lane, Work::Login).await, Ok(Outcome::LoginEnded));
    plant();
    assert_eq!(
        call(&mut lane, Work::Disconnect { logout: true }).await,
        Err(Failure::Cleanup)
    );
    assert!(lane.close(soon()).await);
    assert_eq!(base.calls(), ["--version", "auth login"], "no sign-out");
    let token = base.owned().join("config").join("signed-in");
    assert!(token.exists(), "nothing deleted");
    assert_no_recovery(base, &token).await;
    assert_eq!(fs::read(kept).expect("still there"), KEPT);
}

#[tokio::test]
async fn a_file_hard_linked_at_the_marker_keeps_its_bytes() {
    let base = Base::new("marker-hard-link");
    let kept = kept(&base);
    assert_disconnect_refused(&base, &kept, || {
        fs::hard_link(&kept, base.marker()).expect("a hard link at the marker");
    })
    .await;
}

#[tokio::test]
async fn a_file_symlink_at_the_marker_is_refused_and_its_target_kept() {
    let base = Base::new("marker-symlink");
    let kept = kept(&base);
    assert_disconnect_refused(&base, &kept, || {
        symlink_file(&kept, base.marker()).expect("a file symlink at the marker");
    })
    .await;
}

#[tokio::test]
async fn a_redirected_base_is_refused_before_the_marker() {
    let base = Base::new("marker-redirected");
    let outside = base.0.join("outside");
    fs::create_dir(&outside).expect("outside");
    let there = outside.join(CLAUDE_CODE_DELETING);
    fs::write(&there, KEPT).expect("a file where the marker would land");
    let link = base.0.join("base");
    fs::create_dir(&link).expect("an empty folder");
    set_junction(&link, &outside).expect("a junction for the base");
    let (mut host, _switches) = host(&base, "out");
    host.base = link;
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Disconnect { logout: true }).await,
        Err(Failure::Cleanup)
    );
    assert!(lane.close(soon()).await);
    assert_eq!(fs::read(&there).expect("still there"), KEPT);
    assert!(
        !outside.join(CLAUDE_CODE_LOCK).exists(),
        "nothing was made through it"
    );
    assert_eq!(base.calls(), Vec::<String>::new());
}

/// Version, a sign-in and a Status on the fake in `mode`, then a Disconnect's result.
async fn disconnected(base: &Base, mode: &str) -> Result<Outcome, Failure> {
    let (host, _switches) = host(base, mode);
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert_eq!(call(&mut lane, Work::Login).await, Ok(Outcome::LoginEnded));
    assert_eq!(
        call(&mut lane, Work::Status).await,
        Ok(Outcome::Status {
            version: V,
            signed_in: true
        })
    );
    let disconnected = call(&mut lane, Work::Disconnect { logout: true }).await;
    assert!(lane.close(soon()).await);
    disconnected
}

#[tokio::test]
async fn disconnect_marks_the_folder_signs_out_then_deletes_it() {
    let base = Base::new("disconnect");
    assert_eq!(disconnected(&base, "out").await, Ok(Outcome::Removed));
    assert_eq!(
        base.calls(),
        [
            "--version",
            "auth login",
            "auth status --json",
            "auth logout"
        ]
    );
    assert!(
        base.0.join("logout-saw-marker").exists(),
        "the marker was down before the sign-out"
    );
    assert!(!base.owned().exists());
    assert!(!base.marker().exists());
}

#[tokio::test]
async fn a_failed_sign_out_still_deletes_the_folder() {
    let base = Base::new("logout-fails");
    assert_eq!(
        disconnected(&base, "logoutfail").await,
        Ok(Outcome::Removed)
    );
    assert_eq!(base.calls().last().map(String::as_str), Some("auth logout"));
    assert!(!base.owned().exists());
    assert!(!base.marker().exists());
}

#[tokio::test]
async fn a_marker_that_cant_be_written_signs_nothing_out_and_deletes_nothing() {
    let base = Base::new("marker-fails");
    let (host, _switches) = host(&base, "in");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    fs::create_dir(base.marker()).expect("a folder where the marker goes");
    assert_eq!(
        call(&mut lane, Work::Disconnect { logout: true }).await,
        Err(Failure::Cleanup)
    );
    assert!(lane.close(soon()).await);
    assert_eq!(base.calls(), ["--version"]);
    assert!(base.owned().join("config").exists());
    assert_no_recovery(&base, &base.owned().join("config")).await;
}
#[tokio::test]
async fn a_newer_generation_ends_a_running_sign_in() {
    let base = Base::new("login-fenced");
    let (host, _switches) = host(&base, "loginhang");
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(
        call(&mut lane, Work::Version).await,
        Ok(Outcome::Version(V))
    );
    assert!(lane.submit(0, Work::Login));
    started(&base, "auth login").await;
    let fenced = Instant::now();
    lane.fence(1);
    assert!(lane.submit(1, Work::Status));
    let done = next(&mut lane).await;
    assert_eq!(
        (done.generation, done.result),
        (1, Ok(SIGNED_OUT)),
        "the ended sign-in says nothing"
    );
    assert!(
        fenced.elapsed() < Duration::from_secs(5),
        "ended by the fence, not the fake's 60 s"
    );
    assert!(lane.close(soon()).await);
}

#[tokio::test]
async fn a_lane_that_cant_confirm_a_call_ended_keeps_its_lock_and_starts_nothing_more() {
    let base = Base::new("stuck");
    let (mut host, _switches) = host(&base, "out");
    host.terminate = |_| Err(io::ErrorKind::Other.into());
    let mut lane = Lane::start(host).expect("lane");
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Refused));
    assert_eq!(call(&mut lane, Work::Version).await, Err(Failure::Refused));
    assert_eq!(base.calls(), ["--version"], "nothing started after");
    assert!(!lane.close(soon()).await, "close can't confirm it");
    // shortcut: the kept lock lives until the test process ends, leaving this scratch folder.
    assert!(
        SessionLock::try_acquire(&base.lock()).is_err(),
        "no other brain may take Claude Code over"
    );
}
