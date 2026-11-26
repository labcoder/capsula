use std::io::{Read as _, Write as _};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;

use super::*;
use crate::platform::child::Settled;

const BOUND: Duration = Duration::from_secs(12);

#[cfg(debug_assertions)]
#[path = "brain_parent_fixture.rs"]
mod parent;

fn timed(socket: &UnixStream) {
    no_sigpipe(socket.as_raw_fd()).expect("suppress fixture SIGPIPE");
    socket.set_read_timeout(Some(BOUND)).expect("read deadline");
    socket
        .set_write_timeout(Some(BOUND))
        .expect("write deadline");
}

pub(super) fn clean_stopped_fixture(pid: libc::pid_t) {
    // SAFETY: the failed fixture test still owns and exclusively reaps this stopped direct child.
    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
}

fn read_before(socket: &mut UnixStream, mut buf: &mut [u8], by: Instant) {
    socket
        .set_nonblocking(true)
        .expect("nonblocking fixture read");
    while !buf.is_empty() {
        let left = by.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "fixture's read deadline");
        match socket.read(buf) {
            Ok(0) => panic!("fixture closed before reporting"),
            Ok(read) => buf = &mut buf[read..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                poll(socket, libc::POLLIN, Some(left)).expect("bounded fixture wait");
            }
            Err(error) => panic!("could not read fixture: {error}"),
        }
    }
    socket
        .set_nonblocking(false)
        .expect("restore fixture writes");
}

struct ExitWitness {
    queue: OwnedFd,
    pid: libc::uintptr_t,
}

impl ExitWitness {
    fn arm(pid: u32) -> Self {
        // SAFETY: kqueue returns a new descriptor, not borrowed from another owner.
        let raw = unsafe { libc::kqueue() };
        assert!(raw >= 0, "create exit witness");
        // SAFETY: successful kqueue opened the descriptor and this struct alone owns it.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        let pid = libc::uintptr_t::try_from(pid).expect("pid fits an event identifier");
        let change = libc::kevent {
            ident: pid,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: ptr::null_mut(),
        };
        let now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: one valid change, no event output, and a zero timeout.
        let registered = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &raw const change,
                1,
                ptr::null_mut(),
                0,
                &raw const now,
            )
        };
        assert_eq!(registered, 0, "register exact fixture exit");
        Self { queue, pid }
    }

    fn exited(&self, by: Instant) -> bool {
        let left = by.saturating_duration_since(Instant::now());
        let timeout = libc::timespec {
            tv_sec: libc::time_t::try_from(left.as_secs()).expect("bounded wait seconds"),
            tv_nsec: libc::c_long::from(left.subsec_nanos()),
        };
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        // SAFETY: the output slot is writable; the queue and timeout live through the call.
        let observed = unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                &raw const timeout,
            )
        };
        if observed != 1 {
            return false;
        }
        // SAFETY: a return of one means the output event was initialized.
        let event = unsafe { event.assume_init() };
        event.ident == self.pid
            && event.filter == libc::EVFILT_PROC
            && event.flags & libc::EV_ERROR == 0
            && event.fflags & libc::NOTE_EXIT != 0
    }
}

#[test]
fn forced_retirement_reaches_the_owned_group_but_not_another_session() {
    let by = Instant::now() + BOUND;
    let launch = |script| {
        let (shell, ends) = pair::pairs().expect("fixture sockets");
        let os = Darwin::new(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(script)],
            ends,
        )
        .expect("fixed fixture command");
        let owner = reported(os, Arc::new(SpawnWord::default()), by).expect("fixture owner");
        timed(&shell.control);
        (owner, shell.control)
    };
    let (sentinel, mut other) =
        launch("printf R >&3; IFS= read -r token <&3 && [ \"$token\" = S ]");
    let mut ready = [0_u8; 1];
    read_before(&mut other, &mut ready, by);
    assert_eq!(ready, *b"R", "the other session is ready");
    let (owner, mut control) = launch(
        "/bin/sleep 15 & member=$!; printf '%010d%010d' \"$$\" \"$member\" >&3; wait \"$member\"",
    );
    let mut ids = [0_u8; 20];
    read_before(&mut control, &mut ids, by);
    let read_id = |bytes: &[u8]| std::str::from_utf8(bytes).unwrap().parse::<u32>().unwrap();
    let leader = read_id(&ids[..10]);
    let member = read_id(&ids[10..]);
    let member_pid = libc::pid_t::try_from(member).expect("fixture member pid");
    // SAFETY: just reads the group of the owned fixture's reported member; not signal authority.
    let group = unsafe { libc::getpgid(member_pid) };
    assert_eq!(
        u32::try_from(group).expect("fixture member is alive"),
        leader
    );
    let exited = ExitWitness::arm(member);
    owner.retire();
    assert!(matches!(
        owner.next(by),
        Some(Report::Settled(Settled::Reaped(Exit::Signal(9))))
    ));
    assert!(exited.exited(by), "the same-group member also exited");
    other
        .write_all(b"S\n")
        .expect("the other session still answers");
    assert!(matches!(
        sentinel.next(by),
        Some(Report::Settled(Settled::Reaped(Exit::Code(0))))
    ));
}
