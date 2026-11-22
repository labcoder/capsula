//! A kill-on-close Job Object: when its last handle closes, Windows ends every process still in
//! it, however its holder ended (a crash, Ctrl-C, Task Manager or `taskkill`).

use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, IntoRawHandle as _, OwnedHandle};
#[cfg(test)]
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    TerminateProcess,
};
use windows::core::{BOOL, PCWSTR};

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

    /// The job as a [`Held`] that doesn't hold this process, for checking membership.
    #[cfg(test)]
    pub(crate) fn held(&self) -> Held {
        Held(self.raw())
    }

    /// Puts this process in the job and keeps the handle open until the process exits, so
    /// everything it starts afterwards ends with it.
    pub(crate) fn hold_self(self) -> Result<Held> {
        // SAFETY: plain call returning the pseudo-handle of this live process, never closed.
        let me = unsafe { GetCurrentProcess() };
        // SAFETY: the job handle is open for the call and `me` names this process.
        unsafe { AssignProcessToJobObject(self.raw(), me) }?;
        // Windows closes it at exit, which ends what's left in the job.
        Ok(Held(HANDLE(self.0.into_raw_handle())))
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}

/// The job this process holds itself in; its handle stays open until the process exits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Held(HANDLE);

impl Held {
    /// Ends every other process in the job, whatever started it, and waits up to `limit` until
    /// they're gone.
    pub(crate) fn end_others(self, limit: Duration) -> Result<()> {
        end_all(self.0, std::process::id(), limit)
    }

    /// Whether `process` is in the job.
    pub(crate) fn holds(self, process: &super::Process) -> Result<bool> {
        member(self.0, HANDLE(process.handle.as_raw_handle()))
    }
}

/// Room for more processes than a run starts; a fuller job fails the query instead.
const ROOM: usize = 64;

#[repr(C)]
struct List {
    head: JOBOBJECT_BASIC_PROCESS_ID_LIST,
    rest: [usize; ROOM - 1],
}

/// The ids of the processes running in `job`.
fn members(job: HANDLE) -> Result<Vec<u32>> {
    let mut list = List {
        head: JOBOBJECT_BASIC_PROCESS_ID_LIST::default(),
        rest: [0; ROOM - 1],
    };
    let size = u32::try_from(size_of_val(&list))?;
    // SAFETY: `list` is a process id list with room for ROOM ids, sized and alive for the call.
    unsafe {
        QueryInformationJobObject(
            Some(job),
            JobObjectBasicProcessIdList,
            (&raw mut list).cast(),
            size,
            None,
        )
    }?;
    let count = usize::try_from(list.head.NumberOfProcessIdsInList)?;
    list.head
        .ProcessIdList
        .iter()
        .chain(&list.rest)
        .take(count)
        .map(|&pid| Ok(u32::try_from(pid)?))
        .collect()
}

/// Ends every process in `job` but `except`, whatever started it, and waits up to `limit` until
/// the job lists none; a process outside the job is never touched.
fn end_all(job: HANDLE, except: u32, limit: Duration) -> Result<()> {
    let deadline = Instant::now() + limit;
    let mut failed = String::new();
    loop {
        let left: Vec<u32> = members(job)?
            .into_iter()
            .filter(|&pid| pid != except)
            .collect();
        if left.is_empty() {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "processes {left:?} of the run were still running {limit:?} after they were ended{failed}"
        );
        for pid in left {
            if let Err(error) = terminate(job, pid) {
                failed = format!(" (ending {pid} failed: {error})");
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Ends `pid` while it's in `job`; one that has exited, or whose id now names a process outside
/// the job, is left alone.
fn terminate(job: HANDLE, pid: u32) -> Result<()> {
    // SAFETY: plain call taking a numeric id; a handle it returns is owned below.
    let Ok(raw) = (unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
    }) else {
        return Ok(());
    };
    // SAFETY: the call returned a valid handle that nothing else owns.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let process = HANDLE(process.as_raw_handle());
    if member(job, process)? {
        // SAFETY: the handle is open with terminate access for the call.
        unsafe { TerminateProcess(process, 1) }?;
    }
    Ok(())
}

/// Whether `process` is in `job`.
fn member(job: HANDLE, process: HANDLE) -> Result<bool> {
    let mut member = BOOL::default();
    // SAFETY: both handles are open for the call and `member` is writable.
    unsafe { IsProcessInJob(process, Some(job), &raw mut member) }?;
    Ok(member.as_bool())
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

    #[test]
    fn ending_the_run_ends_every_process_in_its_job_and_nothing_outside_it() {
        let job = Job::kill_on_close().expect("a job");
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
        let mut outsider = Command::new("cmd")
            .arg("/C")
            .raw_arg("set /p go=")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd starts");
        let mut stdin = parent.stdin.take().expect("piped");
        stdin.write_all(b"go\r\n").expect("the parent reads");
        let start = Instant::now();
        while parent.try_wait().expect("reads").is_none() {
            assert!(start.elapsed() < BOUND, "the parent didn't exit");
            thread::sleep(Duration::from_millis(20));
        }
        stdin
            .write_all(b"rem\r\n")
            .expect("the grandchild is alive");
        let mut member = Command::new("cmd")
            .args(["/D", "/C"])
            .raw_arg("set /p go=")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("second member starts");
        job.assign(&member).expect("second member joins");
        assert!(member.try_wait().expect("check second member").is_none());
        let ended = super::end_all(job.raw(), std::process::id(), BOUND);
        let member_exited = member.try_wait().expect("check second member");
        let untouched = outsider.try_wait().expect("reads").is_none();
        if untouched {
            outsider.kill().expect("the outsider ends");
        }
        outsider.wait().expect("the outsider is reaped");
        ended.expect("every member ended");
        assert!(
            member_exited.is_some(),
            "the second member outlived the end"
        );
        assert_eq!(
            stdin.write_all(b"rem\r\n").map_err(|error| error.kind()),
            Err(ErrorKind::BrokenPipe),
            "the grandchild outlived the end"
        );
        assert!(untouched, "a process outside the job was ended");
    }
}
