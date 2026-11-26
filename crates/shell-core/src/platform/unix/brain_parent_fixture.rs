use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

use super::super::*;
use super::{BOUND, ExitWitness, read_before, timed};

const OWNER: &str = "platform::macos::child::group_tests::parent::owner_entry";
const WATCH: &str = "platform::macos::child::group_tests::parent::watch_entry";

fn args(which: &str) -> [&str; 4] {
    ["--exact", which, "--ignored", "--nocapture"]
}

struct FixtureOwner(Child);

impl FixtureOwner {
    fn wait(&mut self, by: Instant) -> Option<ExitStatus> {
        while Instant::now() < by {
            if let Some(status) = self.0.try_wait().expect("wait on the owned shell fixture") {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }
}

impl Drop for FixtureOwner {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let reaped = self.wait(Instant::now() + BOUND).is_some();
            assert!(
                reaped || thread::panicking(),
                "the owned shell fixture did not exit"
            );
        }
    }
}

#[test]
fn isolated_owned_brain_exits_cooperatively_when_its_shell_exits() {
    use kumochi_brain::platform::{Containment, ContainmentTier, Mode, containment, supports};

    assert_eq!(
        containment(Mode::ParentBound),
        Containment::Staged(ContainmentTier::Group)
    );
    assert_eq!(containment(Mode::Detached), Containment::Unsupported);
    assert!(!supports(Mode::ParentBound) && !supports(Mode::Detached));

    let (mut brain, brain_leg) = UnixStream::pair().expect("direct fixture channel");
    let (mut owner, owner_leg) = UnixStream::pair().expect("owner release channel");
    timed(&brain);
    timed(&owner);
    let mut parent = FixtureOwner(
        Command::new(std::env::current_exe().expect("fixture executable"))
            .args(args(OWNER))
            .env_clear()
            .stdin(Stdio::from(OwnedFd::from(brain_leg)))
            .stderr(Stdio::from(OwnedFd::from(owner_leg)))
            .stdout(Stdio::null())
            .spawn()
            .expect("start the owned shell fixture"),
    );
    let by = Instant::now() + BOUND;
    let mut ready = [0_u8; 16];
    read_before(&mut brain, &mut ready, by);
    let ids = ready.as_chunks::<4>().0;
    let [leader, group, session, parent_id] =
        std::array::from_fn(|index| u32::from_ne_bytes(ids[index]));
    assert_eq!(
        parent_id,
        parent.0.id(),
        "the child watches the owning shell"
    );
    assert!(
        leader > 1 && group == leader && session == leader,
        "private group and session"
    );
    // SAFETY: getsid(0) reads the calling process's current session.
    let outside = unsafe { libc::getsid(0) };
    assert_ne!(
        session,
        u32::try_from(outside).expect("valid outer session")
    );
    let exited = ExitWitness::arm(leader);
    owner.write_all(b"X").expect("let the shell exit");
    assert!(parent.wait(by).expect("shell exited").success());
    let mut settled = [0_u8; 1];
    read_before(&mut brain, &mut settled, by);
    assert_eq!(settled, *b"E", "parent loss closed the brain's commit gate");
    assert!(
        exited.exited(by),
        "independent kernel witness of the brain's exit"
    );
}

#[test]
#[ignore = "entry for the owned shell process"]
fn owner_entry() {
    if !std::env::args_os().skip(1).eq(args(OWNER)) {
        return;
    }
    let mut stop = UnixStream::from(
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .expect("release socket"),
    );
    timed(&stop);
    let (_, mut ends) = pair::pairs().expect("fixture leg");
    ends.control = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .expect("brain fixture socket");
    no_sigpipe(ends.control.as_raw_fd()).expect("suppress SIGPIPE");
    let entry = args(WATCH).map(OsStr::new);
    let os = Darwin::new(
        &std::env::current_exe().expect("fixture executable"),
        &entry,
        ends,
    )
    .expect("fixed fixture launch");
    let owner = reported(os, Arc::new(SpawnWord::default()), Instant::now() + BOUND)
        .expect("real shell child owner");
    let mut release = [0_u8; 1];
    stop.read_exact(&mut release)
        .expect("outer witness is armed");
    assert_eq!(release, *b"X");
    std::hint::black_box(&owner);
    std::process::exit(0);
}

#[test]
#[ignore = "entry for the owned brain process"]
fn watch_entry() {
    if !std::env::args_os().skip(1).eq(args(WATCH)) {
        return;
    }
    // SAFETY: only the exact owned-child invocation takes the control leg at fd 3.
    let mut control = UnixStream::from(unsafe { OwnedFd::from_raw_fd(pair::CONTROL_FD) });
    timed(&control);
    let by = Instant::now() + BOUND;
    kumochi_brain::platform::group_parent_fixture(by, || {
        // SAFETY: POSIX getters with no external inputs or output pointers.
        let group = unsafe { libc::getpgrp() };
        // SAFETY: getsid(0) reads the calling process's session.
        let session = unsafe { libc::getsid(0) };
        // SAFETY: getppid only reads the calling process's current parent.
        let parent = unsafe { libc::getppid() };
        control.write_all(&std::process::id().to_ne_bytes())?;
        for id in [group, session, parent] {
            control.write_all(&u32::try_from(id).map_err(io::Error::other)?.to_ne_bytes())?;
        }
        Ok(())
    })
    .expect("the actual parent watch closes admission");
    control.write_all(b"E").expect("confirmed cooperative exit");
}
