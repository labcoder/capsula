use std::io;

/// A private session and process group created by spawning its own leader.
#[derive(Debug)]
pub(in crate::platform) struct OwnedGroup {
    leader: libc::pid_t,
}

/// Darwin's public <sys/spawn.h> flag, not exported by the pinned libc crate.
pub(in crate::platform) const SET_SESSION: libc::c_short = 0x0400;

pub(in crate::platform) enum RetirementError {
    Lost,
    Refused(io::Error),
}

impl OwnedGroup {
    pub(in crate::platform) fn from_spawn(leader: libc::pid_t) -> io::Result<Self> {
        if leader <= 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Self { leader })
    }

    pub(in crate::platform) fn leader(&self) -> libc::pid_t {
        self.leader
    }

    pub(in crate::platform) fn retire(&self) -> Result<(), RetirementError> {
        let id = libc::id_t::try_from(self.leader).map_err(|_| RetirementError::Lost)?;
        // SAFETY: siginfo_t can hold an all-zero value; Darwin may leave it untouched.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        // SAFETY: info is writable, and the exact-child nonblocking wait doesn't reap.
        if unsafe { libc::waitid(libc::P_PID, id, &raw mut info, options) } != 0 {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::ECHILD) {
                RetirementError::Lost
            } else {
                RetirementError::Refused(error)
            });
        }
        if info.si_pid == self.leader
            && matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            )
        {
            return Ok(());
        }
        if info.si_pid != 0
            && (info.si_pid != self.leader
                || !matches!(info.si_code, libc::CLD_STOPPED | libc::CLD_CONTINUED))
        {
            return Err(RetirementError::Lost);
        }
        // SAFETY: this owner is the sole reaper; the unreaped leader reserves its positive
        // group id, and a new session prevents unrelated processes from joining it.
        if unsafe { libc::killpg(self.leader, libc::SIGKILL) } != 0 {
            return Err(RetirementError::Refused(io::Error::last_os_error()));
        }
        Ok(())
    }
}
