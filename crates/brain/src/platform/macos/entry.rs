//! What the executable hands parent-bound startup: the inherited pair's two descriptors, owned.

use std::os::fd::OwnedFd;

/// The inherited control and data descriptors, owned. Only the executable makes one, from its
/// descriptors 3 and 4, so the library never takes a process descriptor itself.
#[derive(Debug)]
pub struct InheritedFds {
    control: OwnedFd,
    data: OwnedFd,
}

impl InheritedFds {
    /// The control and data legs' descriptors.
    #[must_use]
    pub fn new(control: OwnedFd, data: OwnedFd) -> Self {
        Self { control, data }
    }

    /// Checks both descriptors as the inherited pair; needs no runtime.
    ///
    /// # Errors
    /// As [`super::pair::adopt`].
    pub(crate) fn adopt(self) -> Result<super::pair::InheritedPair, super::pair::PairError> {
        super::pair::adopt(self.control, self.data)
    }
}

/// Readies the process before admission: `/` as its working folder, so it holds no folder busy,
/// and core dumps off for good, so no key in its memory reaches one.
///
/// # Errors
/// Either system call's failure.
pub(crate) fn harden() -> std::io::Result<()> {
    use rustix::process::{Resource, Rlimit, chdir, setrlimit};

    chdir("/")?;
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use rustix::process::{Resource, getrlimit};

    use super::{InheritedFds, harden};
    use crate::{ParentBoundError, run_parent_bound};

    const HARDEN_CHILD: &str = "KUMOCHI_TEST_HARDEN_CHILD";
    const HARDENED: i32 = 77;

    /// Run only by the test below, as a child: hardening changes the whole process.
    #[test]
    #[ignore = "subprocess fixture that changes its own working folder and core limit"]
    fn harden_child() {
        if std::env::var_os(HARDEN_CHILD).is_none() {
            return;
        }
        assert_ne!(
            std::env::current_dir().expect("working folder"),
            Path::new("/")
        );
        harden().expect("hardened");
        assert_eq!(
            std::env::current_dir().expect("working folder"),
            Path::new("/")
        );
        let core = getrlimit(Resource::Core);
        assert_eq!((core.current, core.maximum), (Some(0), Some(0)));
        std::process::exit(HARDENED);
    }

    #[test]
    fn hardening_moves_to_the_root_and_turns_core_dumps_off_for_good() {
        #[expect(
            clippy::disallowed_methods,
            reason = "this macOS test runs its own fixture child, which inherits no handles"
        )]
        let child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--ignored",
                "--exact",
                "platform::macos::entry::tests::harden_child",
            ])
            .env_clear()
            .env(HARDEN_CHILD, "1")
            .output()
            .expect("run the fixture child");
        assert_eq!(child.status.code(), Some(HARDENED), "{child:?}");
    }

    #[test]
    fn parent_bound_library_entry_always_refuses() {
        let (control, shell_control) = UnixStream::pair().expect("control pair");
        let (data, shell_data) = UnixStream::pair().expect("data pair");
        for end in [&shell_control, &shell_data] {
            end.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
        }
        let fds = InheritedFds::new(control.into(), data.into());
        assert_eq!(run_parent_bound(fds), Err(ParentBoundError::Unsupported));
        for mut end in [shell_control, shell_data] {
            let mut byte = [0; 1];
            assert_eq!(end.read(&mut byte).expect("the brain's end is closed"), 0);
        }
    }
}
