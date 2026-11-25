use std::io;
use std::sync::Arc;
use tokio::sync::watch;

#[cfg(target_os = "macos")]
use super::macos::parent::ParentWatch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "parent-bound runtime is not wired yet")
)]
pub(crate) enum Verdict {
    Watching,
    Exited,
    Unconfirmed(Unconfirmed),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "only Darwin arms a parent watch")
)]
pub(crate) enum Unconfirmed {
    #[error("parent lineage changed or could not be confirmed")]
    Lineage,
    #[error("parent watch registration was not confirmed (errno {0:?})")]
    Register(Option<i32>),
    #[error("parent watch did not complete")]
    Watch,
    #[error("parent watch thread did not start (errno {0:?})")]
    Thread(Option<i32>),
}

impl Verdict {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "parent-bound runtime is not wired yet")
    )]
    pub(crate) async fn wait(receiver: &mut watch::Receiver<Self>) -> Self {
        receiver
            .wait_for(|verdict| *verdict != Self::Watching)
            .await
            .map_or(Self::Unconfirmed(Unconfirmed::Watch), |verdict| *verdict)
    }
}

#[derive(Clone)]
pub(crate) struct ShellProcess {
    pid: u32,
    owner: Arc<Owner>,
}

enum Owner {
    #[cfg(windows)]
    Native(std::os::windows::io::OwnedHandle),
    #[cfg(target_os = "macos")]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "parent-bound admission is not wired yet")
    )]
    Parent(ParentWatch),
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

    #[cfg(target_os = "macos")]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "parent-bound admission is not wired yet")
    )]
    pub(in crate::platform) fn arm_parent() -> Result<Self, Unconfirmed> {
        let (pid, parent) = ParentWatch::arm()?;
        Ok(Self {
            pid,
            owner: Arc::new(Owner::Parent(parent)),
        })
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "parent-bound runtime is not wired yet")
    )]
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        expect(clippy::unnecessary_wraps, reason = "detached platforms have no watch")
    )]
    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            clippy::unused_self,
            reason = "only macOS has a parent watch; the instance API is shared"
        )
    )]
    pub(crate) fn loss(&self) -> Option<watch::Receiver<Verdict>> {
        #[cfg(target_os = "macos")]
        {
            match *self.owner {
                Owner::Parent(ref parent) => Some(parent.loss()),
                #[cfg(test)]
                Owner::Fixture(_) => None,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    pub(crate) fn register(&self) -> super::connected_shell::Registration<'static> {
        super::register_shell(self.pid)
    }

    pub(crate) fn exited(&self) -> io::Result<bool> {
        match *self.owner {
            #[cfg(windows)]
            Owner::Native(ref handle) => {
                use std::os::windows::io::AsHandle as _;
                super::has_exited(handle.as_handle())
            }
            #[cfg(target_os = "macos")]
            Owner::Parent(ref parent) => match parent.snapshot() {
                Verdict::Watching => Ok(false),
                Verdict::Exited => Ok(true),
                Verdict::Unconfirmed(reason) => Err(io::Error::other(reason)),
            },
            #[cfg(test)]
            Owner::Fixture(ref state) => match state.load(std::sync::atomic::Ordering::Acquire) {
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

#[cfg(test)]
mod tests {
    use super::{ShellProcess, Unconfirmed, Verdict, watch};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[tokio::test]
    async fn late_receiver_sees_the_retained_terminal_value() {
        let (send, initial) = watch::channel(Verdict::Watching);
        drop(initial);
        assert_eq!(send.send_replace(Verdict::Exited), Verdict::Watching);
        let mut late = send.subscribe();
        drop(send);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), Verdict::wait(&mut late)).await,
            Ok(Verdict::Exited)
        );
    }

    #[tokio::test]
    async fn subscribing_during_publication_never_misses_the_verdict() {
        let (send, initial) = watch::channel(Verdict::Watching);
        drop(initial);
        let barrier = Barrier::new(2);
        let mut receiver = std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                assert_eq!(send.send_replace(Verdict::Exited), Verdict::Watching);
            });
            barrier.wait();
            send.subscribe()
        });
        drop(send);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), Verdict::wait(&mut receiver)).await,
            Ok(Verdict::Exited)
        );
    }

    #[tokio::test]
    async fn closed_while_watching_is_unconfirmed_and_a_fixture_has_no_watch() {
        let (send, mut receiver) = watch::channel(Verdict::Watching);
        drop(send);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), Verdict::wait(&mut receiver)).await,
            Ok(Verdict::Unconfirmed(Unconfirmed::Watch))
        );
        let detached = ShellProcess::fixture(23, Arc::new(std::sync::atomic::AtomicU8::new(0)));
        assert!(detached.loss().is_none(), "a fixture has no parent watch");
    }
}
