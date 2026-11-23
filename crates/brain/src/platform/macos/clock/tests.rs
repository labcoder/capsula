use std::process::{Command, Stdio};
use std::time::Instant;

use super::*;

const CHILD_TEST: &str = "platform::macos::clock::tests::clock_child";
const DEADLINE: &str = "TEST_SCREEN_CLOCK_DEADLINE";
const READING: &str = "remaining_ns=";

#[test]
fn deadline_stamps_are_nanoseconds_and_refuse_overflow() {
    assert_eq!(stamp(10, Duration::ZERO), Some(10));
    assert_eq!(stamp(10, Duration::from_nanos(1)), Some(11));
    assert_eq!(stamp(10, Duration::new(1, 1)), Some(1_000_000_011));
    assert_eq!(stamp(u64::MAX - 1, Duration::from_nanos(1)), Some(u64::MAX));
    assert_eq!(stamp(u64::MAX, Duration::from_nanos(1)), None);
    assert_eq!(stamp(0, Duration::MAX), None);
}

#[test]
#[ignore = "subprocess entry; the parent gives it the stamp"]
fn clock_child() {
    let deadline = std::env::var(DEADLINE).unwrap().parse().unwrap();
    let left = qpc_left(deadline).expect("the parent's deadline is still ahead");
    println!("\n{READING}{}", left.as_nanos());
}

#[test]
fn a_child_reads_the_parents_deadline_on_the_same_clock() {
    let budget = Duration::from_secs(60);
    let stamp = qpc_at(budget).expect("stamp the future deadline");
    let stop = Instant::now() + Duration::from_secs(10);
    let before = qpc_left(stamp).expect("read before spawning the child");
    assert!(before > Duration::ZERO && before <= budget, "{before:?}");
    #[expect(clippy::disallowed_methods, reason = "spawns while holding spawn_lock")]
    let mut child = {
        let _spawning =
            crate::platform::spawn_lock_until(&|| stop.checked_duration_since(Instant::now()))
                .expect("acquire the spawn lock");
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
            .env_clear()
            .env(DEADLINE, stamp.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the headless clock child")
    };
    while child.try_wait().expect("check the clock child").is_none() {
        if Instant::now() >= stop {
            if child.kill().is_err() {
                assert!(
                    child.try_wait().unwrap().is_some(),
                    "clock child could not be stopped"
                );
            }
            child.wait().expect("reap the clock child");
            panic!("clock child exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let result = child
        .wait_with_output()
        .expect("read the headless clock child");
    let after = qpc_left(stamp).expect("read after reaping the child");
    assert!(
        result.status.success(),
        "clock child exited with {}",
        result.status
    );
    let sampled: u128 = std::str::from_utf8(&result.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(READING))
        .expect("child reported its clock reading")
        .parse()
        .unwrap();
    assert!(
        (after.as_nanos() < sampled) && (sampled < before.as_nanos()),
        "child read {sampled}ns outside the parent's open {after:?}..{before:?} bracket"
    );
}
