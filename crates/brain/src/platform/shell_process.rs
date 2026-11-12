use std::io;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct ShellProcess {
    pid: u32,
    owner: Arc<Owner>,
}

enum Owner {
    #[cfg(windows)]
    Native(std::os::windows::io::OwnedHandle),
    #[cfg(test)]
    Fixture(Arc<std::sync::atomic::AtomicU8>),
}

impl std::fmt::Debug for ShellProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ShellProcess(..)")
    }
}

impl ShellProcess {
    #[cfg(windows)]
    pub(super) fn new(pid: u32, handle: std::os::windows::io::OwnedHandle) -> Self {
        Self {
            pid,
            owner: Arc::new(Owner::Native(handle)),
        }
    }

    pub(crate) fn exited(&self) -> io::Result<bool> {
        match self.owner.as_ref() {
            #[cfg(windows)]
            Owner::Native(handle) => {
                use std::os::windows::io::AsHandle as _;
                super::has_exited(handle.as_handle())
            }
            #[cfg(test)]
            Owner::Fixture(state) => match state.load(std::sync::atomic::Ordering::Acquire) {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(io::ErrorKind::Other.into()),
            },
        }
    }

    pub(crate) fn same_live_process(&self, other: &Self) -> io::Result<bool> {
        Ok(self.pid == other.pid && !other.exited()? && !self.exited()?)
    }

    #[cfg(test)]
    pub(crate) fn fixture(pid: u32, state: Arc<std::sync::atomic::AtomicU8>) -> Self {
        Self {
            pid,
            owner: Arc::new(Owner::Fixture(state)),
        }
    }
}
