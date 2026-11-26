//! The executable's platform step: owning the inherited pair's descriptors. Only macOS starts a
//! brain on an inherited pair.

use kumochi_brain::platform::InheritedFds;

/// Descriptors 3 and 4 weren't both open.
pub(crate) struct Unclaimed;

/// Takes descriptors 3 (control) and 4 (data) once both are open, and neither otherwise. Private to
/// the executable and called once, by `main` at startup, before anything in the process makes an
/// owner for either. Whether they really are the shell's pair is for adoption and the handshake.
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "checks and takes ownership of the two inherited descriptors"
)]
pub(crate) fn claim_inherited() -> Result<InheritedFds, Unclaimed> {
    use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

    const CONTROL: RawFd = 3;
    const DATA: RawFd = 4;
    // SAFETY: F_GETFD only reads the descriptor's flags; no memory is passed.
    let control_open = unsafe { libc::fcntl(CONTROL, libc::F_GETFD) } != -1;
    // SAFETY: as above.
    let data_open = unsafe { libc::fcntl(DATA, libc::F_GETFD) } != -1;
    if !(control_open && data_open) {
        return Err(Unclaimed);
    }
    // SAFETY: 3 is open, and this private function runs once, at startup, before anything in the
    // process makes an owner for 3, so this handle is its only owner.
    let control = unsafe { OwnedFd::from_raw_fd(CONTROL) };
    // SAFETY: as above, for 4.
    let data = unsafe { OwnedFd::from_raw_fd(DATA) };
    Ok(InheritedFds::new(control, data))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn claim_inherited() -> Result<InheritedFds, Unclaimed> {
    Err(Unclaimed)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::os::fd::{AsRawFd as _, IntoRawFd as _};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::Command;

    const CLAIM_CHILD: &str = "KUMOCHI_TEST_CLAIM_CHILD";

    /// Run only by the test below, as a child with nothing on 3 or 4: it opens a pair onto them,
    /// gives up its ownership of 3, closes 4 and goes through the parent-bound entry, which must
    /// leave 3 open.
    #[test]
    #[ignore = "subprocess fixture that owns descriptors 3 and 4"]
    fn missing_data_leg_child() {
        if std::env::var_os(CLAIM_CHILD).is_none() {
            return;
        }
        let (control, data) = UnixStream::pair().expect("socketpair");
        assert_eq!((control.as_raw_fd(), data.as_raw_fd()), (3, 4));
        let _unowned = control.into_raw_fd();
        drop(data);
        let before = open_descriptors();
        let code = crate::parent_bound();
        assert!(Path::new("/dev/fd/3").exists(), "a refused claim closed 3");
        assert!(!Path::new("/dev/fd/4").exists(), "4 stays closed");
        assert_eq!(
            open_descriptors(),
            before,
            "a refused claim leaves no parent watch, lock or other descriptor open"
        );
        std::process::exit(i32::from(code));
    }

    /// This process's open descriptors, listing's own included, in order.
    fn open_descriptors() -> Vec<u32> {
        let mut open: Vec<u32> = std::fs::read_dir("/dev/fd")
            .expect("the descriptor folder")
            .map(|entry| {
                let name = entry.expect("an entry").file_name();
                name.to_str()
                    .and_then(|name| name.parse().ok())
                    .expect("a descriptor number")
            })
            .collect();
        open.sort_unstable();
        open
    }

    #[test]
    fn a_parent_bound_start_missing_its_data_leg_is_refused() {
        #[expect(
            clippy::disallowed_methods,
            reason = "this macOS test runs its own fixture child, which inherits no handles"
        )]
        let child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--ignored",
                "--exact",
                "platform::tests::missing_data_leg_child",
            ])
            .env_clear()
            .env(CLAIM_CHILD, "1")
            .output()
            .expect("run the fixture child");
        assert_eq!(child.status.code(), Some(76), "{child:?}");
    }
}
