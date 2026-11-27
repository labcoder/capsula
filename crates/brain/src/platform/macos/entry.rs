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
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use kumochi_protocol::constants::ProductIdentity;

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

    const ENTRY_CHILD: &str = "KUMOCHI_TEST_ENTRY_CHILD";

    /// Run only by the test below, as a child, since an admitted entry hardens the whole
    /// process. Its descriptors aren't sockets, so adoption refuses before the parent watch and
    /// custody, and nothing reaches Application Support or the Keychain.
    #[test]
    #[ignore = "subprocess fixture that may harden its own process"]
    fn library_entry_child() {
        if std::env::var_os(ENTRY_CHILD).is_none() {
            return;
        }
        let null = || OwnedFd::from(File::open("/dev/null").expect("the null device"));
        let code = match run_parent_bound(InheritedFds::new(null(), null())) {
            Err(ParentBoundError::Unsupported) => 69,
            Err(ParentBoundError::Refused) => 76,
            _ => 1,
        };
        std::process::exit(code);
    }

    #[test]
    fn the_library_entry_refuses_unless_admitted_and_then_refuses_descriptors_that_arent_a_pair() {
        #[expect(
            clippy::disallowed_methods,
            reason = "this macOS test runs its own fixture child, which inherits no handles"
        )]
        let mut child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--ignored",
                "--exact",
                "platform::macos::entry::tests::library_entry_child",
            ])
            .env_clear()
            .env(ENTRY_CHILD, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the fixture child");
        let by = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("the child's status") {
                break status;
            }
            if Instant::now() >= by {
                let stopped =
                    child.kill().is_ok() || child.try_wait().expect("the child's status").is_some();
                assert!(stopped, "couldn't stop the entry child");
                let reap_by = Instant::now() + Duration::from_secs(2);
                while child.try_wait().expect("the child's status").is_none() {
                    assert!(Instant::now() < reap_by, "the entry child didn't exit");
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("the entry child didn't end by its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Only the first-light Dev build admits parent-bound; Gate and Release are unsupported.
        let refused =
            if cfg!(feature = "first-light") && ProductIdentity::ACTIVE == ProductIdentity::DEV {
                76
            } else {
                69
            };
        assert_eq!(status.code(), Some(refused));
    }
}
