use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{Custody, Helper, Job, classify, faults, reaps_itself};
use crate::test_support::Scratch;

const BOUND: Duration = Duration::from_secs(10);
const SH: &str = "/bin/sh";

/// A shell script the worker runs as `/bin/sh <script>`.
fn script(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(scratch.dir()).expect("scratch");
    let path = scratch.dir().join(name);
    std::fs::write(&path, body).expect("script");
    path
}

fn spawn(job: &Job, script: &Path) -> (Helper, super::ChildPipes) {
    Job::spawn_piped(Path::new(SH), script.to_str().expect("UTF-8"), &[job]).expect("spawn")
}

/// Whether a process with this pid exists, a zombie included.
fn exists(pid: u32) -> bool {
    let pid = libc::pid_t::try_from(pid).expect("a pid fits");
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn gone_within(pid: u32, bound: Duration) -> bool {
    let by = Instant::now() + bound;
    while exists(pid) {
        if Instant::now() >= by {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
fn only_an_exit_or_a_fatal_signal_is_a_reap() {
    let pid = 42;
    let stop = (libc::SIGSTOP << 8) | 0o177;
    let continued = (0x13 << 8) | 0o177;
    let cases = [
        ((0, 0, 0), Some(Custody::Running)),
        ((pid, 3 << 8, 0), Some(Custody::Reaped(3 << 8))),
        (
            (pid, libc::SIGKILL, 0),
            Some(Custody::Reaped(libc::SIGKILL)),
        ),
        ((pid, stop, 0), Some(Custody::Running)),
        ((pid, continued, 0), Some(Custody::Running)),
        ((-1, 0, libc::EINTR), None),
        ((-1, 0, libc::ECHILD), Some(Custody::Lost)),
        ((-1, 0, libc::EINVAL), Some(Custody::Lost)),
    ];
    for ((waited, status, errno), expected) in cases {
        assert_eq!(
            classify(waited, status, errno),
            expected,
            "waitpid {waited}, status {status:#x}, errno {errno}"
        );
    }
}

#[test]
fn an_ignored_sigchld_or_nocldwait_reaps_children_itself() {
    assert!(!reaps_itself(libc::SIG_DFL, 0));
    assert!(reaps_itself(libc::SIG_IGN, 0));
    assert!(reaps_itself(libc::SIG_DFL, libc::SA_NOCLDWAIT));
    assert!(!reaps_itself(1234, libc::SA_RESTART));
    assert!(!super::auto_reaps(), "the test process keeps statuses");
}

#[test]
fn a_worker_gets_only_its_pipes_and_null_stderr_with_no_environment_in_the_root() {
    let scratch = Scratch::new("mac-job-fds");
    // SAFETY: duplicating standard error is a plain call; F_DUPFD leaves the copy inheritable.
    let raw = unsafe { libc::fcntl(2, libc::F_DUPFD, 20) };
    assert!(raw >= 20, "a foreign descriptor");
    // SAFETY: fcntl just returned this descriptor, and nothing else owns it.
    let foreign = unsafe { OwnedFd::from_raw_fd(raw) };
    let body = format!(
        "for fd in 3 4 5 6 7 8 9 {}; do [ -e /dev/fd/$fd ] && exit 7; done\n\
         [ -z \"${{HOME+set}}${{TMPDIR+set}}\" ] || exit 8\n\
         [ \"$(pwd)\" = / ] || exit 9\n\
         [ -c /dev/fd/2 ] || exit 10\n\
         read line; printf '%s!' \"$line\"\n",
        foreign.as_raw_fd()
    );
    let job = Job::new().expect("job");
    let (helper, mut pipes) = spawn(&job, &script(&scratch, "fds.sh", &body));
    pipes.stdin.write_all(b"ping\n").expect("write");
    drop(pipes.stdin);
    let mut out = String::new();
    pipes.stdout.read_to_string(&mut out).expect("read");
    assert!(helper.exits_within(BOUND).expect("wait"));
    assert_eq!(helper.exit_code().expect("code"), 0);
    assert_eq!(out, "ping!");
    drop(foreign);
}

#[test]
fn a_running_worker_reports_no_code_and_a_killed_one_reads_as_its_signal() {
    let scratch = Scratch::new("mac-job-code");
    let job = Job::new().expect("job");
    let (helper, _pipes) = spawn(&job, &script(&scratch, "sleep.sh", "exec /bin/sleep 30\n"));
    assert!(!helper.exits_within(Duration::ZERO).expect("wait"));
    assert_eq!(
        helper.exit_code().expect_err("still running").kind(),
        std::io::ErrorKind::WouldBlock
    );
    helper.terminate().expect("kill");
    assert!(helper.exits_within(BOUND).expect("wait"));
    assert_eq!(helper.exit_code().expect("code"), 128 + 9);
}

#[test]
fn dropping_the_last_helper_leaves_the_reap_to_the_job() {
    let scratch = Scratch::new("mac-job-helper-drop");
    let job = Job::new().expect("job");
    let (helper, pipes) = spawn(&job, &script(&scratch, "sleep.sh", "exec /bin/sleep 30\n"));
    let pid = helper.pid();
    drop((helper, pipes));
    assert!(exists(pid), "the worker outlives its handle");
    job.terminate().expect("terminate");
    assert!(!exists(pid), "terminate reaped it");
}

#[test]
fn dropping_the_last_job_and_helper_still_ends_and_reaps_a_live_worker() {
    let scratch = Scratch::new("mac-job-drop");
    let job = Job::new().expect("job");
    let (helper, pipes) = spawn(&job, &script(&scratch, "sleep.sh", "exec /bin/sleep 30\n"));
    let pid = helper.pid();
    drop((helper, pipes, job));
    assert!(gone_within(pid, BOUND), "the reaper killed and reaped it");
}

#[test]
fn a_reaped_worker_is_never_signalled_even_by_racing_callers() {
    let scratch = Scratch::new("mac-job-race");
    let job = Job::new().expect("job");
    let (exited, mut pipes) = spawn(&job, &script(&scratch, "exit.sh", "read line; exit 3\n"));
    pipes.stdin.write_all(b"\n").expect("write");
    assert!(exited.exits_within(BOUND).expect("wait"));
    let (running, _pipes) = spawn(&job, &script(&scratch, "sleep.sh", "exec /bin/sleep 30\n"));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..50 {
                    exited.terminate().expect("a reaped worker isn't signalled");
                    running.terminate().expect("kill or already reaped");
                    running.exits_within(Duration::ZERO).expect("wait");
                }
            });
        }
    });
    assert!(running.exits_within(BOUND).expect("wait"));
    assert_eq!(exited.exit_code().expect("code"), 3);
    assert_eq!(running.exit_code().expect("code"), 128 + 9);
    job.terminate().expect("nothing left");
}

#[test]
fn a_reaper_that_cannot_start_leaves_no_job() {
    let error = Job::start(|_| Err(std::io::Error::other("no thread"))).expect_err("no job");
    assert_eq!(error.to_string(), "no thread");
}

#[test]
fn a_shutdown_racing_spawns_either_refuses_them_or_reaps_them() {
    let scratch = Scratch::new("mac-job-shutdown");
    let sleeper = script(&scratch, "sleep.sh", "exec /bin/sleep 30\n");
    let job = Job::new().expect("job");
    let started = AtomicBool::new(false);
    let (helpers, refusal) = std::thread::scope(|scope| {
        let spawner = scope.spawn(|| {
            let mut helpers = Vec::new();
            for _ in 0..10_000 {
                match Job::spawn_piped(Path::new(SH), sleeper.to_str().expect("UTF-8"), &[&job]) {
                    Ok((helper, _)) => helpers.push(helper),
                    Err(error) => return (helpers, Some(error)),
                }
                started.store(true, Ordering::Release);
            }
            (helpers, None)
        });
        while !started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        job.shut_down().expect("shut down");
        spawner.join().expect("spawner")
    });
    assert!(job.is_shut_down());
    assert!(!helpers.is_empty());
    assert_eq!(
        refusal.expect("a refusal").to_string(),
        "the job was shut down"
    );
    for helper in &helpers {
        assert!(!exists(helper.pid()), "every registered worker was reaped");
    }
    assert!(Job::spawn_piped(Path::new(SH), "", &[&job]).is_err());
}

#[test]
fn a_worker_that_cant_be_killed_is_reported_not_counted_as_ended() {
    let scratch = Scratch::new("mac-job-unkillable");
    let job = Job::new().expect("job");
    let (helper, _pipes) = spawn(&job, &script(&scratch, "sleep.sh", "exec /bin/sleep 30\n"));
    faults::REFUSE_KILL.set(true);
    let terminated = job.terminate();
    let shut = job.shut_down();
    faults::REFUSE_KILL.set(false);
    assert_eq!(
        terminated.expect_err("refused").kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        shut.expect_err("refused").kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(!helper.exits_within(Duration::ZERO).expect("wait"));
    job.terminate().expect("terminate");
    assert!(helper.exits_within(Duration::ZERO).expect("reaped"));
}

#[test]
fn anything_but_one_job_refuses_before_a_worker_starts() {
    let (first, second) = (Job::new().expect("job"), Job::new().expect("job"));
    for jobs in [&[][..], &[&first, &second][..]] {
        assert_eq!(
            Job::spawn_piped(Path::new(SH), "", jobs)
                .expect_err("refused")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        Job::spawn_piped(Path::new("sh"), "", &[&first])
            .expect_err("relative")
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
fn a_kernel_that_reaps_children_itself_refuses_spawns_and_signals() {
    let scratch = Scratch::new("mac-job-sigchld");
    let sleeper = script(&scratch, "sleep.sh", "exec /bin/sleep 30\n");
    let job = Job::new().expect("job");
    let (helper, _pipes) = spawn(&job, &sleeper);
    faults::REAPS_ITSELF.set(true);
    let spawned = Job::spawn_piped(Path::new(SH), sleeper.to_str().expect("UTF-8"), &[&job]);
    let killed = helper.terminate();
    faults::REAPS_ITSELF.set(false);
    assert_eq!(
        spawned.expect_err("refused").kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        killed.expect_err("refused").kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(!helper.exits_within(Duration::ZERO).expect("wait"));
    job.terminate().expect("terminate");
}
