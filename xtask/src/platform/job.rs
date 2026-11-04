//! A kill-on-close Job Object: when its last handle closes, Windows ends every process still in
//! it, however its holder ended (a crash, Ctrl-C, Task Manager or `taskkill`).

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, IntoRawHandle as _, OwnedHandle};
#[cfg(test)]
use std::process::Child;

use anyhow::Result;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::PCWSTR;

/// The job; dropping it ends everything in it.
#[derive(Debug)]
pub(crate) struct Job(OwnedHandle);

impl Job {
    pub(crate) fn kill_on_close() -> Result<Self> {
        // SAFETY: plain call creating an unnamed job with default security.
        let raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
        // SAFETY: the call returned a valid handle that nothing else owns.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw.0) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of_val(&limits))?;
        // SAFETY: `limits` is the struct this information class names, sized and alive for the call.
        unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size,
            )
        }?;
        Ok(job)
    }

    /// Puts `child` in the job; what it starts from then on joins too.
    #[cfg(test)]
    pub(crate) fn assign(&self, child: &Child) -> Result<()> {
        // SAFETY: both handles are open for the call, the child's owned by `child`.
        unsafe { AssignProcessToJobObject(self.raw(), HANDLE(child.as_raw_handle())) }?;
        Ok(())
    }

    /// Puts this process in the job and keeps the handle open until the process exits, so
    /// everything it starts afterwards ends with it.
    pub(crate) fn hold_self(self) -> Result<()> {
        // SAFETY: plain call returning the pseudo-handle of this live process, never closed.
        let me = unsafe { GetCurrentProcess() };
        // SAFETY: the job handle is open for the call and `me` names this process.
        unsafe { AssignProcessToJobObject(self.raw(), me) }?;
        // Windows closes it at exit, which ends what's left in the job.
        let _open_until_exit = self.0.into_raw_handle();
        Ok(())
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Write as _};
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::Job;

    const BOUND: Duration = Duration::from_secs(10);

    #[test]
    fn closing_the_job_ends_a_grandchild_whose_parent_exited() {
        let job = Job::kill_on_close().expect("a job");
        // The parent waits for one line, starts a grandchild sharing its stdin, and exits.
        let mut parent = Command::new("cmd")
            .arg("/C")
            .raw_arg("set /p go= & start /B cmd")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd starts");
        job.assign(&parent)
            .expect("assigned before it starts anything");
        let mut stdin = parent.stdin.take().expect("piped");
        stdin.write_all(b"go\r\n").expect("the parent reads");
        let start = Instant::now();
        while parent.try_wait().expect("reads").is_none() {
            assert!(start.elapsed() < BOUND, "the parent didn't exit");
            thread::sleep(Duration::from_millis(20));
        }
        // Only the grandchild still holds the pipe's read end, so a write proves it alive.
        stdin
            .write_all(b"rem\r\n")
            .expect("the grandchild is alive");
        drop(job);
        let start = Instant::now();
        loop {
            match stdin.write_all(b"rem\r\n") {
                Err(error) if error.kind() == ErrorKind::BrokenPipe => break,
                result => result.expect("a live reader or a broken pipe"),
            }
            assert!(start.elapsed() < BOUND, "the grandchild outlived the job");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
