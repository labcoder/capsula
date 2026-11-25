use std::io::{self, PipeReader, PipeWriter};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::ptr;
use std::thread::{self, JoinHandle};
use tokio::sync::watch;

use super::super::shell_process::{Unconfirmed, Verdict};

const TICK: libc::timespec = libc::timespec {
    tv_sec: 1,
    tv_nsec: 0,
};
const NOW: libc::timespec = libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

pub(in crate::platform) struct ParentWatch {
    verdict: watch::Receiver<Verdict>,
    stop: Option<(PipeWriter, JoinHandle<()>)>,
}

struct Armed {
    pid: u32,
    queue: OwnedFd,
    stop: PipeReader,
}

impl ParentWatch {
    pub(in crate::platform) fn arm() -> Result<(u32, Self), Unconfirmed> {
        Self::arm_with(parent_id, |armed, verdict| {
            thread::Builder::new()
                .name("parent-watch".into())
                .spawn(move || armed.run(&verdict, parent_id))
        })
    }

    fn arm_with(
        parent: impl Fn() -> u32,
        start: impl FnOnce(Armed, watch::Sender<Verdict>) -> io::Result<JoinHandle<()>>,
    ) -> Result<(u32, Self), Unconfirmed> {
        let pid = parent();
        if pid <= 1 {
            return Err(Unconfirmed::Lineage);
        }

        let queue = queue().map_err(|_| Unconfirmed::Watch)?;
        let proc = change(pid_ident(pid), libc::EVFILT_PROC, libc::NOTE_EXIT);
        receipt(&queue, &proc).map_err(|error| Unconfirmed::Register(error.raw_os_error()))?;
        if parent() != pid {
            return Err(Unconfirmed::Lineage);
        }

        let (stop, write) = {
            let _spawn = super::super::spawn_lock();
            io::pipe().map_err(|_| Unconfirmed::Watch)?
        };
        let read = change(stop_ident(&stop), libc::EVFILT_READ, 0);
        receipt(&queue, &read).map_err(|_| Unconfirmed::Watch)?;
        let (send, verdict) = watch::channel(Verdict::Watching);
        let worker = start(Armed { pid, queue, stop }, send)
            .map_err(|error| Unconfirmed::Thread(error.raw_os_error()))?;
        Ok((
            pid,
            Self {
                verdict,
                stop: Some((write, worker)),
            },
        ))
    }

    pub(in crate::platform) fn loss(&self) -> watch::Receiver<Verdict> {
        self.verdict.clone()
    }

    pub(in crate::platform) fn snapshot(&self) -> Verdict {
        let closed = self.verdict.has_changed().is_err();
        match *self.verdict.borrow() {
            Verdict::Watching if closed => Verdict::Unconfirmed(Unconfirmed::Watch),
            verdict => verdict,
        }
    }
}

impl Drop for ParentWatch {
    fn drop(&mut self) {
        if let Some((write, worker)) = self.stop.take() {
            drop(write);
            drop(worker.join());
        }
    }
}

impl Armed {
    fn run(self, verdict: &watch::Sender<Verdict>, parent: impl Fn() -> u32) {
        if let Some(terminal) = wait(
            self.pid,
            stop_ident(&self.stop),
            |timeout| poll(&self.queue, None, timeout),
            parent,
        ) {
            verdict.send_replace(terminal);
        }
    }
}

fn wait(
    pid: u32,
    stop: usize,
    mut event: impl FnMut(&libc::timespec) -> io::Result<Option<libc::kevent>>,
    parent: impl Fn() -> u32,
) -> Option<Verdict> {
    let mut lineage_changed = false;
    loop {
        match event(if lineage_changed { &NOW } else { &TICK }) {
            Ok(Some(note)) if note.flags & libc::EV_ERROR == 0 => {
                if note.ident == pid_ident(pid)
                    && note.filter == libc::EVFILT_PROC
                    && note.fflags & libc::NOTE_EXIT != 0
                {
                    return Some(Verdict::Exited);
                }
                if note.ident == stop
                    && note.filter == libc::EVFILT_READ
                    && note.flags & libc::EV_EOF != 0
                {
                    return None;
                }
            }
            Ok(None) => {}
            _ => return Some(Verdict::Unconfirmed(Unconfirmed::Watch)),
        }
        if lineage_changed {
            return Some(Verdict::Unconfirmed(Unconfirmed::Lineage));
        }
        lineage_changed = parent() != pid;
    }
}

fn parent_id() -> u32 {
    // SAFETY: getppid has no inputs and returns the calling process's parent ID.
    let pid = unsafe { libc::getppid() };
    u32::try_from(pid).unwrap_or(1)
}

fn pid_ident(pid: u32) -> usize {
    usize::try_from(pid).expect("a Darwin event ident can hold a process ID")
}

fn stop_ident(stop: &PipeReader) -> usize {
    usize::try_from(stop.as_raw_fd()).expect("an owned descriptor is nonnegative")
}

fn queue() -> io::Result<OwnedFd> {
    let _spawn = super::super::spawn_lock();
    // SAFETY: kqueue has no inputs and returns a fresh descriptor on success.
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful kqueue call transferred sole ownership of this descriptor.
    let queue = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: queue owns fd; F_SETFD takes an integer flag word as its third argument.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(queue)
}

fn change(ident: usize, filter: i16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD | libc::EV_RECEIPT,
        fflags,
        data: 0,
        udata: ptr::null_mut(),
    }
}

fn receipt(queue: &OwnedFd, change: &libc::kevent) -> io::Result<()> {
    confirm_receipt(change, poll(queue, Some(change), &NOW)?)
}

fn confirm_receipt(change: &libc::kevent, note: Option<libc::kevent>) -> io::Result<()> {
    let Some(note) = note else {
        return Err(io::ErrorKind::InvalidData.into());
    };
    if note.ident != change.ident
        || note.filter != change.filter
        || note.flags & libc::EV_ERROR == 0
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let errno = i32::try_from(note.data).map_err(|_| io::ErrorKind::InvalidData)?;
    match errno {
        0 => Ok(()),
        1.. => Err(io::Error::from_raw_os_error(errno)),
        _ => Err(io::ErrorKind::InvalidData.into()),
    }
}

fn poll(
    queue: &OwnedFd,
    change: Option<&libc::kevent>,
    timeout: &libc::timespec,
) -> io::Result<Option<libc::kevent>> {
    let mut note = std::mem::MaybeUninit::uninit();
    // SAFETY: queue is live; the change is absent or readable, note has room for one event,
    // and timeout is readable for this call.
    let count = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            change.map_or(ptr::null(), ptr::from_ref),
            i32::from(change.is_some()),
            note.as_mut_ptr(),
            1,
            timeout,
        )
    };
    match count {
        0 => Ok(None),
        // SAFETY: kevent returned one event, so it initialized note.
        1 => Ok(Some(unsafe { note.assume_init() })),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests;
