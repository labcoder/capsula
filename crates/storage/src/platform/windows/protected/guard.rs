//! The setup guard: one named mutex per user and edition in the global namespace, so every
//! session of this user contends on the same object and nothing is written to disk. It carries a
//! medium label that stops lower-integrity code, this user's included, from opening it to take,
//! read or change it. An existing mutex is used only if it's trusted like a held folder: a trusted
//! owner, that label and policy, and no one else allowed to take, release or re-secure it. A
//! planted or loosened one is `Unavailable`, never `Busy`. `Busy` means another handle holds
//! this verified object, not who holds it, so a caller only refuses or waits. The guard doesn't
//! coordinate older Kumochi builds, which don't know it.

use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use kumochi_protocol::constants::OBJECT_PREFIX;
use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{DELETE, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, ReleaseMutex, WaitForSingleObject,
};

use windows_sys::Win32::System::SystemServices::{
    SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP, SYSTEM_MANDATORY_LABEL_NO_READ_UP,
    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};

use super::{Aces, Descriptor, Security, Trusted, user_sid};

/// The rights that take, release or re-secure the guard. Generic rights are judged with the file
/// mapping, which grants each of these wherever the mutex mapping does.
const TAKES: u32 = SYNCHRONIZE | MUTEX_MODIFY_STATE | WRITE_DAC | WRITE_OWNER | DELETE;

/// The label policy the guard needs: no-execute-up is what denies lower code `SYNCHRONIZE`.
const FENCE: u32 = SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
    | SYSTEM_MANDATORY_LABEL_NO_READ_UP
    | SYSTEM_MANDATORY_LABEL_NO_EXECUTE_UP;

/// Why the setup guard wasn't taken. It carries no name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    /// Another handle holds the verified guard. It says nothing about who, so it only ever
    /// means refuse or wait.
    #[error("another session holds the setup guard")]
    Busy,
    /// It couldn't be created, opened or trusted.
    #[error("the setup guard can't be verified")]
    Unavailable,
}

/// Holds the setup guard until dropped, on the thread that took it. Like any mutex it's
/// re-entrant on that thread, so a process takes it once.
#[derive(Debug)]
#[must_use = "dropping the guard releases it"]
pub struct SetupGuard {
    mutex: OwnedHandle,
    _thread: PhantomData<*const ()>,
}

/// Takes this user's setup guard without waiting. A guard whose last holder exited without
/// releasing it is taken: whatever that holder left is for the inspection under the new guard.
pub fn acquire_setup_guard() -> Result<SetupGuard, GuardError> {
    let user = user_sid().map_err(|_| GuardError::Unavailable)?;
    acquire(&format!(r"Global\{OBJECT_PREFIX}-setup-{user}"), &user)
}

fn acquire(name: &str, user: &str) -> Result<SetupGuard, GuardError> {
    let descriptor = Descriptor::new(&sddl(user)).map_err(|_| GuardError::Unavailable)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| GuardError::Unavailable)?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and `attributes` points to a live descriptor.
    let handle = unsafe { CreateMutexW(&raw const attributes, 0, wide.as_ptr()) };
    if handle.is_null() {
        return Err(GuardError::Unavailable);
    }
    // SAFETY: a fresh handle owned only here.
    let mutex = unsafe { OwnedHandle::from_raw_handle(handle) };
    if !trusted(&mutex).unwrap_or(false) {
        return Err(GuardError::Unavailable);
    }
    // SAFETY: the handle is open for the call, which doesn't wait.
    match unsafe { WaitForSingleObject(mutex.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(SetupGuard {
            mutex,
            _thread: PhantomData,
        }),
        WAIT_TIMEOUT => Err(GuardError::Busy),
        _ => Err(GuardError::Unavailable),
    }
}

/// Only `user` may use the guard, and code below medium may not open it.
fn sddl(user: &str) -> String {
    format!("D:P(A;;GA;;;{user})S:(ML;;NWNRNX;;;ME)")
}

/// Whether the mutex has a trusted owner, a medium or higher label with the whole fence, and no ACE
/// letting someone untrusted take, release or re-secure it.
fn trusted(mutex: &OwnedHandle) -> Result<bool, super::Untrusted> {
    let trusted = Trusted::new()?;
    let security = Security::read_object(mutex.as_raw_handle(), SE_KERNEL_OBJECT)?;
    Ok(trusted.contains(security.owner)
        && !security.open_from_below(FENCE)?
        && !security.others_may(TAKES, &trusted, Aces::Effective)?)
}

#[cfg(test)]
impl SetupGuard {
    /// Closes the handle without releasing, so the mutex is abandoned when this thread exits.
    fn abandon(self) {
        let guard = std::mem::ManuallyDrop::new(self);
        // SAFETY: the handle is read out once and the guard's own drop never runs.
        drop(unsafe { std::ptr::read(&raw const guard.mutex) });
    }
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        // SAFETY: the guard can't leave the thread that owns the mutex.
        let released = unsafe { ReleaseMutex(self.mutex.as_raw_handle()) };
        debug_assert_ne!(released, 0, "the owning thread releases the setup guard");
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError};
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, GetLengthSid, RevertToSelf, SID_AND_ATTRIBUTES, SecurityImpersonation,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
        TOKEN_MANDATORY_LABEL, TokenImpersonation, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenMutexW, OpenProcessToken, SetThreadToken,
    };

    use super::super::Sid;
    use super::*;

    /// This user's SID, for tests to expect.
    fn this_user() -> String {
        user_sid().expect("this user's SID")
    }

    fn name(case: &str) -> String {
        format!(
            r"Global\{OBJECT_PREFIX}-test-setup-{}-{case}",
            std::process::id()
        )
    }

    /// Takes the guard on another thread and drops it there.
    fn elsewhere(name: &str) -> Result<(), GuardError> {
        let name = name.to_owned();
        thread::spawn(move || acquire(&name, &this_user()).map(drop))
            .join()
            .expect("the other thread finishes")
    }

    /// Runs `work` on a thread impersonating this process's own token lowered to low integrity.
    fn at_low_integrity<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        thread::spawn(move || {
            let mut process = std::ptr::null_mut();
            // SAFETY: returns a pseudo-handle that needs no closing.
            let me = unsafe { GetCurrentProcess() };
            // SAFETY: `me` is this process and `process` is writable.
            let opened = unsafe { OpenProcessToken(me, TOKEN_DUPLICATE, &raw mut process) };
            assert_ne!(opened, 0, "the process token opens");
            // SAFETY: a fresh handle owned only here.
            let process = unsafe { OwnedHandle::from_raw_handle(process) };
            let mut token = std::ptr::null_mut();
            // SAFETY: `process` is open for duplication and `token` is writable.
            let duplicated = unsafe {
                DuplicateTokenEx(
                    process.as_raw_handle(),
                    TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT,
                    std::ptr::null(),
                    SecurityImpersonation,
                    TokenImpersonation,
                    &raw mut token,
                )
            };
            assert_ne!(duplicated, 0, "the token duplicates");
            // SAFETY: a fresh handle owned only here.
            let token = unsafe { OwnedHandle::from_raw_handle(token) };
            let low = Sid::parse("S-1-16-4096").expect("the low label parses");
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: low.0,
                    Attributes: SE_GROUP_INTEGRITY.cast_unsigned(),
                },
            };
            // SAFETY: `low` is a valid SID.
            let sid_length = unsafe { GetLengthSid(low.0) };
            let length = u32::try_from(size_of::<TOKEN_MANDATORY_LABEL>()).expect("small");
            // SAFETY: `label` and the SID it points to are live for the call.
            let lowered = unsafe {
                SetTokenInformation(
                    token.as_raw_handle(),
                    TokenIntegrityLevel,
                    (&raw const label).cast(),
                    length + sid_length,
                )
            };
            assert_ne!(lowered, 0, "the copy is lowered to low integrity");
            // SAFETY: a null thread is this one, and the token is open for impersonation.
            let impersonating = unsafe { SetThreadToken(std::ptr::null(), token.as_raw_handle()) };
            assert_ne!(impersonating, 0, "this thread runs at low integrity");
            let done = work();
            // SAFETY: ends this thread's impersonation.
            let reverted = unsafe { RevertToSelf() };
            assert_ne!(reverted, 0, "the thread reverts");
            done
        })
        .join()
        .expect("the low-integrity thread finishes")
    }

    /// A mutex already at `name` with `sddl`'s security, held open by the result.
    fn planted(name: &str, sddl: &str) -> OwnedHandle {
        let descriptor = Descriptor::new(sddl).expect("the SDDL parses");
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("small"),
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and `attributes` points to a live descriptor.
        let handle = unsafe { CreateMutexW(&raw const attributes, 0, wide.as_ptr()) };
        assert!(!handle.is_null(), "the planted mutex is created");
        // SAFETY: a fresh handle owned only here.
        unsafe { OwnedHandle::from_raw_handle(handle) }
    }

    #[test]
    fn a_held_guard_is_busy_elsewhere_until_released() {
        let name = name("held");
        let user = this_user();
        let _keeper = planted(&name, &sddl(&user));
        let guard = acquire(&name, &user).expect("the first holder takes it");
        assert_eq!(elsewhere(&name), Err(GuardError::Busy));
        drop(guard);
        assert_eq!(elsewhere(&name), Ok(()), "the same object is free again");
    }

    #[test]
    fn a_guard_whose_holder_exited_is_taken() {
        let name = name("abandoned");
        let user = this_user();
        let _keeper = planted(&name, &sddl(&user));
        let held = name.clone();
        thread::spawn(move || {
            acquire(&held, &this_user())
                .expect("the exiting thread takes the guard")
                .abandon();
        })
        .join()
        .expect("the holder exits");
        assert_eq!(elsewhere(&name), Ok(()), "the abandoned object is taken");
    }

    #[test]
    fn lower_integrity_code_of_this_user_can_neither_open_nor_take_the_guard() {
        let name = name("low");
        let user = this_user();
        let _guard = acquire(&name, &user).expect("this process takes it");
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let (handle, error) = at_low_integrity(move || {
            // SAFETY: `wide` is NUL-terminated.
            let handle = unsafe { OpenMutexW(SYNCHRONIZE, 0, wide.as_ptr()) };
            // SAFETY: reads this thread's last error right after the call.
            let error = unsafe { GetLastError() };
            if !handle.is_null() {
                // SAFETY: a fresh handle owned only here.
                drop(unsafe { OwnedHandle::from_raw_handle(handle) });
            }
            (!handle.is_null(), error)
        });
        assert_eq!((handle, error), (false, ERROR_ACCESS_DENIED));
        let taken = at_low_integrity(move || acquire(&name, &user).map(drop));
        assert_eq!(taken, Err(GuardError::Unavailable));
    }

    #[test]
    fn a_guard_others_may_take_or_lower_code_may_reach_is_unavailable() {
        let user = this_user();
        for (case, sddl) in [
            (
                "everyone",
                format!("D:P(A;;GA;;;{user})(A;;0x100000;;;WD)S:(ML;;NWNRNX;;;ME)"),
            ),
            (
                "low-label",
                format!("D:P(A;;GA;;;{user})S:(ML;;NWNRNX;;;LW)"),
            ),
            ("unlabelled", format!("D:P(A;;GA;;;{user})")),
            (
                "no-execute-up-missing",
                format!("D:P(A;;GA;;;{user})S:(ML;;NWNR;;;ME)"),
            ),
        ] {
            let name = name(case);
            let _planted = planted(&name, &sddl);
            assert_eq!(
                acquire(&name, &user).map(drop),
                Err(GuardError::Unavailable),
                "{case}"
            );
        }
    }
    const HOLDER: &str = "STORAGE_TEST_GUARD_HOLDER";

    /// The holder process. Dropping it stops it, and outside a panic a holder that won't exit
    /// fails.
    struct Holder(std::process::Child);

    impl Holder {
        /// Kills the holder, leaving its input open, and waits at most 10 s for its exit status.
        fn kill(&mut self) -> Option<std::process::ExitStatus> {
            self.0.kill().ok();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if let Ok(Some(status)) = self.0.try_wait() {
                    return Some(status);
                }
                thread::sleep(std::time::Duration::from_millis(20));
            }
            None
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            drop(self.0.stdin.take());
            let exited = self.kill().is_some();
            if !exited {
                eprintln!("the holder didn't exit");
            }
            assert!(exited || thread::panicking(), "the holder didn't exit");
        }
    }

    /// Runs `work` on its own thread and fails if it takes longer than 30 s, so a blocked read or
    /// take ends the test instead of wedging it.
    fn within_deadline<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sent, result) = std::sync::mpsc::channel();
        thread::spawn(move || sent.send(work()).ok());
        result
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("finished within the deadline")
    }

    /// The child half of the cross-process test, spawned only by it with the guard's name in
    /// `HOLDER`; without it there is nothing to hold. Holds the guard until killed.
    #[test]
    #[ignore = "spawned by a_guard_held_by_another_process_is_busy_until_that_process_is_killed"]
    fn guard_holder() {
        use std::io::{Read as _, Write as _};

        let Some(name) = std::env::var_os(HOLDER) else {
            return;
        };
        let name = name.into_string().expect("a guard name");
        let _guard = acquire(&name, &this_user()).expect("the holder takes the guard");
        // The test harness has already written this test's name without ending the line.
        let mut out = std::io::stdout();
        out.write_all(b"\nguard-held\n")
            .and_then(|()| out.flush())
            .expect("report holding");
        std::io::stdin()
            .read_to_end(&mut Vec::new())
            .expect("wait until the parent ends");
    }

    #[test]
    fn a_guard_held_by_another_process_is_busy_until_that_process_is_killed() {
        use std::io::BufRead as _;
        use std::process::{Command, Stdio};

        let name = name("process");
        let user = this_user();
        let _keeper = planted(&name, &sddl(&user));
        let mut holder = Holder(
            Command::new(std::env::current_exe().expect("this test binary"))
                .args([
                    "--exact",
                    "platform::windows::protected::guard::tests::guard_holder",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(HOLDER, &name)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn the holder"),
        );
        let output = holder.0.stdout.take().expect("the holder's piped output");
        assert!(
            within_deadline(move || std::io::BufReader::new(output)
                .lines()
                .map_while(Result::ok)
                .any(|line| line == "guard-held")),
            "the holder reported taking the guard"
        );
        let (probe, prober) = (name.clone(), user.clone());
        assert_eq!(
            within_deadline(move || acquire(&probe, &prober).map(drop)),
            Err(GuardError::Busy),
            "another process holds it"
        );
        let status = holder.kill().expect("the holder exits once killed");
        assert!(!status.success(), "the holder was killed");
        assert_eq!(
            within_deadline(move || acquire(&name, &user).map(drop)),
            Ok(()),
            "the killed holder's guard is taken"
        );
    }
}
