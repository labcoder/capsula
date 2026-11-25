use std::cell::Cell;
use std::io::{Read as _, Write as _};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::{
    ParentWatch, Unconfirmed, Verdict, change, confirm_receipt, io, parent_id, thread, wait, watch,
};
use crate::platform::ShellProcess;

const PID: u32 = 42;
const STOP: usize = 3;
const BOUND: Duration = Duration::from_secs(5);

fn exit_note() -> libc::kevent {
    libc::kevent {
        flags: 0,
        ..change(42, libc::EVFILT_PROC, libc::NOTE_EXIT)
    }
}

#[test]
fn receipt_must_acknowledge_the_exact_change() {
    let registration = change(42, libc::EVFILT_PROC, libc::NOTE_EXIT);
    let receipt = libc::kevent {
        flags: libc::EV_ERROR,
        ..registration
    };
    confirm_receipt(&registration, Some(receipt)).expect("zero errno acknowledges registration");
    let refused = libc::kevent {
        data: isize::try_from(libc::ESRCH).unwrap(),
        ..receipt
    };
    assert_eq!(
        confirm_receipt(&registration, Some(refused))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ESRCH)
    );
    for invalid in [
        None,
        Some(exit_note()),
        Some(libc::kevent {
            ident: 43,
            ..receipt
        }),
        Some(libc::kevent {
            filter: libc::EVFILT_READ,
            ..receipt
        }),
    ] {
        assert_eq!(
            confirm_receipt(&registration, invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn lineage_gap_gets_one_final_poll_for_the_original_note() {
    for (note, expected) in [
        (None, Verdict::Unconfirmed(Unconfirmed::Lineage)),
        (Some(exit_note()), Verdict::Exited),
        (
            Some(libc::kevent {
                ident: 43,
                ..exit_note()
            }),
            Verdict::Unconfirmed(Unconfirmed::Lineage),
        ),
        (
            Some(libc::kevent {
                filter: libc::EVFILT_READ,
                ..exit_note()
            }),
            Verdict::Unconfirmed(Unconfirmed::Lineage),
        ),
        (
            Some(libc::kevent {
                fflags: 0,
                ..exit_note()
            }),
            Verdict::Unconfirmed(Unconfirmed::Lineage),
        ),
        (
            Some(libc::kevent {
                flags: libc::EV_ERROR,
                ..exit_note()
            }),
            Verdict::Unconfirmed(Unconfirmed::Watch),
        ),
    ] {
        let mut polls = [(1, None), (0, note)].into_iter();
        let result = wait(
            PID,
            STOP,
            |timeout| {
                let (seconds, note) = polls.next().expect("no poll after the final one");
                assert_eq!((timeout.tv_sec, timeout.tv_nsec), (seconds, 0));
                Ok(note)
            },
            || 1,
        );
        assert_eq!(result, Some(expected));
        assert_eq!(polls.count(), 0, "lineage loss must make the final poll");
    }
    assert_eq!(
        wait(
            PID,
            STOP,
            |_| Err(io::ErrorKind::Interrupted.into()),
            || PID
        ),
        Some(Verdict::Unconfirmed(Unconfirmed::Watch))
    );
}

#[tokio::test]
async fn eof_and_exit_note_are_independent_in_both_orders() {
    for eof_first in [true, false] {
        let (mut read, write) = io::pipe().expect("owned transport");
        let mut write = Some(write);
        let (send, mut loss) = watch::channel(Verdict::Watching);
        if eof_first {
            drop(write.take());
            assert_eq!(read.read(&mut [0]).unwrap(), 0);
            assert!(matches!(loss.has_changed(), Ok(false)));
        }
        let verdict = wait(PID, STOP, |_| Ok(Some(exit_note())), || panic!("note wins"));
        send.send_replace(verdict.expect("an exit note is terminal"));
        assert_eq!(
            tokio::time::timeout(BOUND, Verdict::wait(&mut loss)).await,
            Ok(Verdict::Exited)
        );
        if !eof_first {
            drop(write.take());
            assert_eq!(read.read(&mut [0]).unwrap(), 0);
        }
        assert_eq!(*loss.borrow(), Verdict::Exited);
    }
}

struct OwnedChild(Child);

const CHILD_ARGS: [&str; 4] = [
    "--exact",
    "platform::macos::parent::tests::child_entry",
    "--ignored",
    "--test-threads=1",
];

impl OwnedChild {
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock covers the owned test child launch"
    )]
    fn start() -> Self {
        let child = {
            let _spawn = crate::platform::spawn_lock();
            Command::new(std::env::current_exe().unwrap())
                .args(CHILD_ARGS)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start owned headless fixture")
        };
        let mut child = Self(child);
        child
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&std::process::id().to_ne_bytes())
            .unwrap();
        child
    }

    fn reap(&mut self) -> ExitStatus {
        let deadline = Instant::now() + BOUND;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "owned child timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_success(&mut self) {
        let status = self.reap();
        if !status.success() {
            let mut report = String::new();
            self.0
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut report)
                .unwrap();
            self.0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut report)
                .unwrap();
            panic!("owned fixture exited {status}: {report}");
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            drop(self.0.kill());
            self.0.wait().expect("reap owned fixture");
        }
    }
}

#[tokio::test]
async fn native_idle_exit_wakes_with_transport_still_open() {
    let mut child = OwnedChild::start();
    let pid = child.0.id();
    let (_, owner) = ParentWatch::arm_with(
        || pid,
        |armed, send| thread::Builder::new().spawn(move || armed.run(&send, || pid)),
    )
    .unwrap_or_else(|reason| panic!("owned child watch refused: {reason}"));
    let mut loss = owner.loss();
    let (_read, _write) = io::pipe().expect("keep the transport alive throughout the watch");
    assert!(matches!(loss.has_changed(), Ok(false)));
    drop(child.0.stdin.take());
    assert_eq!(
        tokio::time::timeout(BOUND, Verdict::wait(&mut loss)).await,
        Ok(Verdict::Exited)
    );
    child.assert_success();
    drop(owner);
    assert_eq!(*loss.borrow(), Verdict::Exited);
}

#[tokio::test]
async fn the_watch_latches_its_loss_before_waking_so_a_later_claim_fails() {
    for claim_first in [true, false] {
        let mut child = OwnedChild::start();
        let pid = child.0.id();
        let (_, watch) = ParentWatch::arm_with(
            || pid,
            |armed, send| thread::Builder::new().spawn(move || armed.run(&send, || pid)),
        )
        .unwrap_or_else(|reason| panic!("owned child watch refused: {reason}"));
        let shell = ShellProcess::parent(pid, watch);
        let mut loss = shell.loss().expect("a parent watch has a receiver");
        let claimed = claim_first.then(|| shell.claim_commit().is_some());
        assert_eq!(shell.fence(), Ok(()));
        drop(child.0.stdin.take());
        assert_eq!(
            tokio::time::timeout(BOUND, Verdict::wait(&mut loss)).await,
            Ok(Verdict::Exited)
        );
        child.assert_success();
        assert_eq!(claimed, claim_first.then_some(true));
        assert!(
            shell.claim_commit().is_none(),
            "a claim after the published loss fails"
        );
        assert_eq!(shell.fence(), Err(Verdict::Exited));
    }
}

#[tokio::test]
async fn a_watch_that_closed_while_watching_fails_the_fence_after_the_claim() {
    let pid = std::process::id();
    let (_, watch) = ParentWatch::arm_with(
        || pid,
        |armed, send| thread::Builder::new().spawn(move || drop((armed, send))),
    )
    .unwrap_or_else(|reason| panic!("own process watch refused: {reason}"));
    let shell = ShellProcess::parent(pid, watch);
    let mut loss = shell.loss().expect("a parent watch has a receiver");
    assert_eq!(
        tokio::time::timeout(BOUND, Verdict::wait(&mut loss)).await,
        Ok(Verdict::Unconfirmed(Unconfirmed::Watch))
    );
    assert!(
        shell.claim_commit().is_some(),
        "a watch that never published latched no loss"
    );
    assert_eq!(shell.fence(), Err(Verdict::Unconfirmed(Unconfirmed::Watch)));
}

#[test]
fn arm_refuses_invalid_lineage_and_thread_failure() {
    assert_eq!(
        ParentWatch::arm_with(|| 1, |_, _| panic!("must refuse before start")).err(),
        Some(Unconfirmed::Lineage)
    );
    let mut child = OwnedChild::start();
    let pid = child.0.id();
    let first = Cell::new(Some(pid));
    assert_eq!(
        ParentWatch::arm_with(
            || first.take().unwrap_or(1),
            |_, _| panic!("must refuse before start")
        )
        .err(),
        Some(Unconfirmed::Lineage)
    );
    assert_eq!(
        ParentWatch::arm_with(
            || pid,
            |_, _| Err(io::Error::from_raw_os_error(libc::EAGAIN))
        )
        .err(),
        Some(Unconfirmed::Thread(Some(libc::EAGAIN)))
    );
    drop(child.0.stdin.take());
    child.assert_success();
}

#[tokio::test]
#[ignore = "entry for the owned child; not a standalone test"]
async fn child_entry() {
    if !std::env::args_os().skip(1).eq(CHILD_ARGS) {
        return;
    }
    let mut stdin = io::stdin().lock();
    let mut pid = [0; 4];
    stdin
        .read_exact(&mut pid)
        .expect("receive the parent's PID from the owner");
    assert_eq!(parent_id(), u32::from_ne_bytes(pid));
    let shell = ShellProcess::arm_parent().expect("arm on the live owning process");
    let mut loss = shell.loss().expect("Mac parent-bound shell has a receiver");
    assert_eq!(*loss.borrow(), Verdict::Watching);
    assert!(!shell.exited().unwrap(), "a live parent has not exited");
    drop(shell);
    assert_eq!(
        tokio::time::timeout(BOUND, Verdict::wait(&mut loss)).await,
        Ok(Verdict::Unconfirmed(Unconfirmed::Watch))
    );
    assert_eq!(stdin.read(&mut [0]).unwrap(), 0);
}
