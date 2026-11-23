//! Job Objects: the brain job that ends every helper when the brain dies, and nested run jobs
//! that end one run's processes and nothing else.
//! On macOS, use `posix_spawn` file actions for pipes and process-group kill for termination.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, FALSE, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, TRUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::platform::ChildPipes;

/// Exit code given to processes a job terminates.
const TERMINATED: u32 = 1;
/// How long [`Job::terminate`] waits for the job to empty.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(5);

/// A Job Object that ends its processes when its last handle closes. Only the brain holds the
/// handle, and it's never inherited, so the brain's death ends everything in it.
#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
    /// Set by [`Job::shut_down`]; from then on the job refuses new processes.
    shut: AtomicBool,
}

impl Job {
    /// Creates an empty job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    ///
    /// # Errors
    /// Fails when the OS refuses to create or configure the job.
    pub fn new() -> io::Result<Self> {
        // SAFETY: no attributes (so the handle isn't inheritable) and no name.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh job handle owned only here.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let job = Self {
            handle,
            shut: AtomicBool::new(false),
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the struct this information class expects, with its true size.
        let set = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )
        };
        if set == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    /// Refuses a start into `jobs` once any of them was shut down. The caller holds
    /// [`crate::platform::spawn_lock`] until its process exists.
    pub(super) fn admit(jobs: &[&Self]) -> io::Result<()> {
        if jobs.iter().any(|job| job.is_shut_down()) {
            return Err(io::Error::other("the job was shut down"));
        }
        Ok(())
    }

    /// Processes still running in this job and the jobs nested in it.
    ///
    /// # Errors
    /// Fails when the OS can't report it.
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is the struct this information class expects, with its true size.
        let ok = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_u32::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>(),
                std::ptr::null_mut(),
            )
        };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }

    /// Whether `process` runs in this job or a job nested in it.
    ///
    /// # Errors
    /// Fails when the handle lacks query rights.
    pub fn contains(&self, process: BorrowedHandle<'_>) -> io::Result<bool> {
        let mut inside = FALSE;
        // SAFETY: both handles are open for the borrow and `inside` is a writable BOOL.
        if unsafe { IsProcessInJob(process.as_raw_handle(), self.raw(), &raw mut inside) } == FALSE
        {
            return Err(io::Error::last_os_error());
        }
        Ok(inside != FALSE)
    }

    /// Whether [`Job::shut_down`] has begun.
    pub(crate) fn is_shut_down(&self) -> bool {
        self.shut.load(Ordering::Relaxed)
    }

    /// Refuses new processes from now on, then [`Job::terminate`]s. A start either created its
    /// process before the refusal, under [`crate::platform::spawn_lock`], and so is ended, or
    /// sees the refusal.
    ///
    /// # Errors
    /// As [`Job::terminate`].
    pub(crate) fn shut_down(&self) -> io::Result<()> {
        {
            let _spawning = crate::platform::spawn_lock();
            self.shut.store(true, Ordering::Relaxed);
        }
        self.terminate()
    }

    /// Ends every process in this job and the jobs nested in it, then waits until none is left.
    ///
    /// # Errors
    /// Fails when the OS refuses, or processes are still running after five seconds.
    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: the job handle is open for the borrow.
        if unsafe { TerminateJobObject(self.raw(), TERMINATED) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        let deadline = Instant::now() + EMPTY_TIMEOUT;
        // shortcut: polls each millisecond; a completion port would avoid it, worth it once runs
        // end often enough to matter.
        while self.active_processes()? > 0 {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "job still has processes",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// Starts a helper with piped stdin and stdout; stderr is disconnected.
    /// Only the two child pipe ends are inherited, and their parent copies close before return.
    /// [`crate::platform::spawn_lock`] is held from marking them inheritable until they close.
    ///
    /// # Errors
    /// Fails when pipes or the process can't be created or configured, or a job was shut down.
    pub fn spawn_piped(exe: &Path, arg: &str, jobs: &[&Self]) -> io::Result<(Helper, ChildPipes)> {
        // Taken first so it drops last, after the child ends, on every error path too.
        let spawning = crate::platform::spawn_lock();
        let (child_stdin, stdin) = anonymous_pipe()?;
        let (stdout, child_stdout) = anonymous_pipe()?;
        let inherited = [child_stdin.as_raw_handle(), child_stdout.as_raw_handle()];
        for handle in inherited {
            // SAFETY: each handle is an open child pipe end owned until after process creation.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == FALSE
            {
                return Err(io::Error::last_os_error());
            }
        }
        let helper = Self::spawn_inner(exe, arg, jobs, Some(inherited))?;
        drop((child_stdin, child_stdout));
        drop(spawning);
        Ok((helper, ChildPipes { stdin, stdout }))
    }

    fn spawn_inner(
        exe: &Path,
        arg: &str,
        jobs: &[&Self],
        stdio: Option<[HANDLE; 2]>,
    ) -> io::Result<Helper> {
        Self::admit(jobs)?;
        let mut application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command_line: Vec<u16> = format!("\"{}\" {arg}", exe.display())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handles: Vec<HANDLE> = jobs.iter().map(|job| job.raw()).collect();
        let inherited = stdio.as_ref().map_or(&[][..], |handles| handles.as_slice());
        let attributes = AttributeList::with_jobs(&handles, inherited)?;
        let mut startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_u32::<STARTUPINFOEXW>(),
                ..Default::default()
            },
            lpAttributeList: attributes.0.as_ptr().cast_mut().cast(),
        };
        if let Some([stdin, stdout]) = stdio {
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = stdin;
            startup.StartupInfo.hStdOutput = stdout;
        }
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: both strings are NUL-terminated and writable as CreateProcessW requires; the
        // attribute list and its handles outlive the call; only the listed pipe ends inherit.
        let created = unsafe {
            CreateProcessW(
                application.as_mut_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                if stdio.is_some() { TRUE } else { FALSE },
                EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                (&raw const startup).cast(),
                &raw mut info,
            )
        };
        if created == FALSE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateProcessW returned both handles, owned only here; the thread one closes now.
        drop(unsafe { OwnedHandle::from_raw_handle(info.hThread) });
        // SAFETY: as above.
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        Ok(Helper {
            process,
            pid: info.dwProcessId,
        })
    }
}

fn anonymous_pipe() -> io::Result<(File, File)> {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: both output pointers are valid; null attributes make both ends non-inheritable.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == FALSE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned a fresh read handle, owned only here.
    let read = unsafe { File::from_raw_handle(read) };
    // SAFETY: CreatePipe returned a fresh write handle, owned only here.
    let write = unsafe { File::from_raw_handle(write) };
    Ok((read, write))
}

/// A helper process the brain started.
#[derive(Debug)]
pub struct Helper {
    process: OwnedHandle,
    pid: u32,
}

impl Helper {
    pub(super) const fn new(process: OwnedHandle, pid: u32) -> Self {
        Self { process, pid }
    }

    /// A helper for process `pid` through a handle that can wait on it but not terminate it.
    #[cfg(test)]
    pub(crate) fn watching(pid: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::{
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        let process = super::pipe::open_process(
            pid,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        )?;
        Ok(Self::new(process, pid))
    }

    /// The helper's process id.
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Requests termination; a process that has exited, or is already exiting, counts as success.
    /// Only [`Helper::exits_within`] shows that it ended.
    ///
    /// # Errors
    /// Fails when Windows refuses for any reason but access denied and the process hasn't exited.
    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: the process handle is open for the borrow and has termination access.
        if unsafe { TerminateProcess(self.process.as_raw_handle(), TERMINATED) } != FALSE {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // Windows refuses a process that is already exiting, until its exit completes.
        if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) {
            return Ok(());
        }
        match self.exits_within(Duration::ZERO) {
            Ok(true) => Ok(()),
            _ => Err(error),
        }
    }

    /// The exit code of a process that has exited.
    ///
    /// # Errors
    /// Fails when the OS can't report it.
    pub fn exit_code(&self) -> io::Result<u32> {
        let mut code = 0;
        // SAFETY: the process handle is open for the borrow and `code` is a valid out pointer.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }

    /// Whether the process exits within `timeout`.
    ///
    /// # Errors
    /// Fails when the wait itself fails.
    pub fn exits_within(&self, timeout: Duration) -> io::Result<bool> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the process handle is open for the borrow.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// A process-creation attribute list holding job and optional inherited-handle lists. The buffer is
/// `u64`s so it's aligned for the pointers the OS writes into it.
pub(super) struct AttributeList(pub(super) Vec<u64>);

impl AttributeList {
    pub(super) fn with_jobs(jobs: &[HANDLE], inherited: &[HANDLE]) -> io::Result<Self> {
        let mut size = 0;
        let count = 1 + u32::from(!inherited.is_empty());
        // SAFETY: the sizing call takes a null list and only writes `size`; it reports
        // ERROR_INSUFFICIENT_BUFFER, which is expected.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
        let mut buffer = vec![0_u64; size.div_ceil(size_of::<u64>())];
        // SAFETY: the buffer is at least `size` bytes, as the sizing call asked.
        let initialized = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &raw mut size)
        };
        if initialized == FALSE {
            return Err(io::Error::last_os_error());
        }
        // Only an initialized list gets the destructor.
        let mut list = Self(buffer);
        // SAFETY: `jobs` holds open job handles and outlives every use of the list, which only
        // lives inside one spawn call.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list.ptr(),
                0,
                usize::try_from(PROC_THREAD_ATTRIBUTE_JOB_LIST)
                    .expect("attribute ids fit in usize"),
                jobs.as_ptr().cast::<c_void>(),
                size_of_val(jobs),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if updated == FALSE {
            return Err(io::Error::last_os_error());
        }
        if !inherited.is_empty() {
            // SAFETY: the listed pipe handles and their slice outlive the list and spawn call.
            let updated = unsafe {
                UpdateProcThreadAttribute(
                    list.ptr(),
                    0,
                    usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST)
                        .expect("attribute ids fit in usize"),
                    inherited.as_ptr().cast::<c_void>(),
                    size_of_val(inherited),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if updated == FALSE {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(list)
    }

    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialized in `with_jobs` and is no longer in use.
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

pub(super) fn size_u32<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("Win32 structs are far smaller than 4 GiB")
}
