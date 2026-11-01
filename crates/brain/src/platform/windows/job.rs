//! Job Objects: the brain job that ends every helper when the brain dies, and nested run jobs
//! that end one run's processes and nothing else.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{FALSE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Exit code given to processes a job terminates.
const TERMINATED: u32 = 1;
/// How long [`Job::terminate`] waits for the job to empty.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(5);

/// A Job Object that ends its processes when its last handle closes. Only the brain holds the
/// handle, and it's never inherited, so the brain's death ends everything in it.
#[derive(Debug)]
pub struct Job(OwnedHandle);

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
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
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

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
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

    /// Starts `exe arg` inside `jobs`, from its first instruction, with no window and no
    /// inherited handles. Later jobs nest inside earlier ones: `[brain, run]` puts the process
    /// in a run job nested in the brain job.
    ///
    /// # Errors
    /// Fails when the process can't be created.
    pub fn spawn(exe: &Path, arg: &str, jobs: &[&Self]) -> io::Result<Helper> {
        let mut application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command_line: Vec<u16> = format!("\"{}\" {arg}", exe.display())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handles: Vec<HANDLE> = jobs.iter().map(|job| job.raw()).collect();
        let attributes = AttributeList::with_jobs(&handles)?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_u32::<STARTUPINFOEXW>(),
                ..Default::default()
            },
            lpAttributeList: attributes.0.as_ptr().cast_mut().cast(),
        };
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: both strings are NUL-terminated and writable as CreateProcessW requires; the
        // attribute list and the job handles it points to outlive the call; handles aren't
        // inherited (bInheritHandles is FALSE).
        let created = unsafe {
            CreateProcessW(
                application.as_mut_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                FALSE,
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

/// A helper process the brain started.
#[derive(Debug)]
pub struct Helper {
    process: OwnedHandle,
    pid: u32,
}

impl Helper {
    /// The helper's process id.
    pub const fn pid(&self) -> u32 {
        self.pid
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

/// A process-creation attribute list holding `PROC_THREAD_ATTRIBUTE_JOB_LIST`. The buffer is
/// `u64`s so it's aligned for the pointers the OS writes into it.
struct AttributeList(Vec<u64>);

impl AttributeList {
    fn with_jobs(jobs: &[HANDLE]) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: the sizing call takes a null list and only writes `size`; it reports
        // ERROR_INSUFFICIENT_BUFFER, which is expected.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut size) };
        let mut list = Self(vec![0; size.div_ceil(size_of::<u64>())]);
        // SAFETY: the buffer is at least `size` bytes, as the sizing call asked.
        if unsafe { InitializeProcThreadAttributeList(list.ptr(), 1, 0, &raw mut size) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `jobs` holds open job handles and outlives every use of the list, which only
        // lives inside `Job::spawn`.
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

fn size_u32<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("Win32 structs are far smaller than 4 GiB")
}
