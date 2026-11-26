//! What the executable hands parent-bound startup: the inherited pair's two descriptors, owned.

use std::os::fd::OwnedFd;

/// The inherited control and data descriptors, owned. Only the executable makes one, from its
/// descriptors 3 and 4, so the library never takes a process descriptor itself.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "parent-bound admission adopts them; it isn't wired yet"
)]
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
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::InheritedFds;
    use crate::{ParentBoundError, run_parent_bound};

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
