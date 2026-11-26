use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::watch;

#[cfg(target_os = "macos")]
use super::macos::parent::ParentWatch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(
        test,
        all(target_os = "macos", debug_assertions, feature = "test-support")
    )),
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
        not(any(
            test,
            all(target_os = "macos", debug_assertions, feature = "test-support")
        )),
        expect(dead_code, reason = "parent-bound runtime is not wired yet")
    )]
    pub(crate) async fn wait(receiver: &mut watch::Receiver<Self>) -> Self {
        receiver
            .wait_for(|verdict| *verdict != Self::Watching)
            .await
            .map_or(Self::Unconfirmed(Unconfirmed::Watch), |verdict| *verdict)
    }

    /// [`Verdict::wait`] on a shell's loss receiver; never resolves for a shell without one.
    #[cfg_attr(
        not(any(
            test,
            all(target_os = "macos", debug_assertions, feature = "test-support")
        )),
        expect(dead_code, reason = "parent-bound runtime is not wired yet")
    )]
    pub(crate) async fn lost(loss: &mut Option<watch::Receiver<Self>>) -> Self {
        match loss {
            Some(loss) => Self::wait(loss).await,
            None => std::future::pending().await,
        }
    }
}

const OPEN: u8 = 0;
const CLAIMED: u8 = 1;
const LOST: u8 = 2;

/// Orders the first parent-loss publication against the one commit claim.
#[cfg_attr(
    not(any(test, target_os = "macos")),
    expect(dead_code, reason = "only Darwin arms a parent watch")
)]
pub(in crate::platform) struct CommitGate(AtomicU8);

#[cfg_attr(
    not(any(test, target_os = "macos")),
    expect(dead_code, reason = "only Darwin arms a parent watch")
)]
impl CommitGate {
    pub(in crate::platform) const fn new() -> Self {
        Self(AtomicU8::new(OPEN))
    }

    /// Called before the watch publishes. True when the loss came before any claim.
    pub(in crate::platform) fn lose(&self) -> bool {
        self.swap(LOST)
    }

    fn claim(&self) -> bool {
        self.swap(CLAIMED)
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(in crate::platform) fn lost(&self) -> bool {
        self.0.load(Ordering::Acquire) == LOST
    }

    fn swap(&self, to: u8) -> bool {
        self.0
            .compare_exchange(OPEN, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Held by parent-bound startup after it won the commit claim.
#[cfg_attr(
    not(any(
        test,
        all(target_os = "macos", debug_assertions, feature = "test-support")
    )),
    expect(dead_code, reason = "parent-bound startup is not wired yet")
)]
pub(crate) struct Committed(());

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
        not(any(test, all(debug_assertions, feature = "test-support"))),
        expect(dead_code, reason = "parent-bound admission is not wired yet")
    )]
    Parent(ParentWatch),
    #[cfg(test)]
    Fixture(Arc<AtomicU8>, CommitGate),
    #[cfg(test)]
    Watched(watch::Receiver<Verdict>, CommitGate),
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
        not(any(test, all(debug_assertions, feature = "test-support"))),
        expect(dead_code, reason = "parent-bound admission is not wired yet")
    )]
    pub(in crate::platform) fn arm_parent() -> Result<Self, Unconfirmed> {
        let (pid, parent) = ParentWatch::arm()?;
        Ok(Self::parent(pid, parent))
    }

    #[cfg(target_os = "macos")]
    pub(in crate::platform) fn parent(pid: u32, parent: ParentWatch) -> Self {
        Self {
            pid,
            owner: Arc::new(Owner::Parent(parent)),
        }
    }

    /// The one commit claim: Some at most once, and never after a loss was latched.
    #[cfg_attr(
        not(any(
            test,
            all(target_os = "macos", debug_assertions, feature = "test-support")
        )),
        expect(dead_code, reason = "parent-bound startup is not wired yet")
    )]
    pub(crate) fn claim_commit(&self) -> Option<Committed> {
        match *self.owner {
            #[cfg(windows)]
            Owner::Native(_) => None,
            #[cfg(target_os = "macos")]
            Owner::Parent(ref parent) => parent.gate().claim().then_some(Committed(())),
            #[cfg(test)]
            Owner::Fixture(_, ref gate) | Owner::Watched(_, ref gate) => {
                gate.claim().then_some(Committed(()))
            }
        }
    }

    /// The current verdict, read without waiting: Err once a terminal verdict or a closed watch
    /// is visible. Without a parent watch nothing is confirmed.
    #[cfg_attr(
        not(any(
            test,
            all(target_os = "macos", debug_assertions, feature = "test-support")
        )),
        expect(dead_code, reason = "parent-bound startup is not wired yet")
    )]
    pub(crate) fn fence(&self) -> Result<(), Verdict> {
        match self.snapshot() {
            Verdict::Watching => Ok(()),
            terminal => Err(terminal),
        }
    }

    fn snapshot(&self) -> Verdict {
        match *self.owner {
            #[cfg(windows)]
            Owner::Native(_) => Verdict::Unconfirmed(Unconfirmed::Watch),
            #[cfg(target_os = "macos")]
            Owner::Parent(ref parent) => parent.snapshot(),
            #[cfg(test)]
            Owner::Fixture(ref state, _) => match state.load(Ordering::Acquire) {
                0 => Verdict::Watching,
                1 => Verdict::Exited,
                _ => Verdict::Unconfirmed(Unconfirmed::Watch),
            },
            #[cfg(test)]
            Owner::Watched(ref verdict, _) => {
                let closed = verdict.has_changed().is_err();
                match *verdict.borrow() {
                    Verdict::Watching if closed => Verdict::Unconfirmed(Unconfirmed::Watch),
                    verdict => verdict,
                }
            }
        }
    }

    #[cfg_attr(
        not(any(
            test,
            all(target_os = "macos", debug_assertions, feature = "test-support")
        )),
        expect(dead_code, reason = "parent-bound runtime is not wired yet")
    )]
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        expect(clippy::unnecessary_wraps, reason = "detached platforms have no watch")
    )]
    pub(crate) fn loss(&self) -> Option<watch::Receiver<Verdict>> {
        match *self.owner {
            #[cfg(windows)]
            Owner::Native(_) => None,
            #[cfg(target_os = "macos")]
            Owner::Parent(ref parent) => Some(parent.loss()),
            #[cfg(test)]
            Owner::Fixture(..) => None,
            #[cfg(test)]
            Owner::Watched(ref verdict, _) => Some(verdict.clone()),
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
            Owner::Fixture(ref state, _) => match state.load(Ordering::Acquire) {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(io::ErrorKind::Other.into()),
            },
            #[cfg(test)]
            Owner::Watched(..) => match self.snapshot() {
                Verdict::Watching => Ok(false),
                Verdict::Exited => Ok(true),
                Verdict::Unconfirmed(reason) => Err(io::Error::other(reason)),
            },
        }
    }

    pub(crate) fn same_live_process(&self, other: &Self) -> io::Result<bool> {
        Ok(self.pid == other.pid && !other.exited()? && !self.exited()?)
    }

    #[cfg(test)]
    pub(crate) fn fixture(pid: u32, state: Arc<AtomicU8>) -> Self {
        Self {
            pid,
            owner: Arc::new(Owner::Fixture(state, CommitGate::new())),
        }
    }

    /// A fixture whose verdict a test publishes or closes through the returned sender, the way
    /// the parent watch does.
    #[cfg(test)]
    pub(crate) fn watched(pid: u32) -> (Self, watch::Sender<Verdict>) {
        let (send, verdict) = watch::channel(Verdict::Watching);
        let owner = Arc::new(Owner::Watched(verdict, CommitGate::new()));
        (Self { pid, owner }, send)
    }

    /// Publishes a fixture verdict the way the watch thread does: the loss latch, then the value.
    /// True when the loss came before any claim.
    #[cfg(test)]
    pub(crate) fn publish(&self, verdict: u8) -> bool {
        match *self.owner {
            Owner::Fixture(ref state, ref gate) => {
                let first = gate.lose();
                state.store(verdict, Ordering::Release);
                first
            }
            #[cfg(windows)]
            Owner::Native(_) => panic!("only a fixture publishes on demand"),
            #[cfg(target_os = "macos")]
            Owner::Parent(_) => panic!("only a fixture publishes on demand"),
            Owner::Watched(..) => panic!("only a fixture publishes on demand"),
        }
    }

    /// Publishes `verdict` to a [`ShellProcess::watched`] shell as the parent watch does: latches
    /// the gate first, then wakes every waiter. True when this latched it.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn publish_watched(&self, send: &watch::Sender<Verdict>, verdict: Verdict) -> bool {
        let first = match *self.owner {
            Owner::Watched(_, ref gate) => gate.lose(),
            #[cfg(windows)]
            Owner::Native(_) => panic!("only a watched fixture publishes a verdict"),
            #[cfg(target_os = "macos")]
            Owner::Parent(_) => panic!("only a watched fixture publishes a verdict"),
            Owner::Fixture(..) => panic!("only a watched fixture publishes a verdict"),
        };
        send.send_replace(verdict);
        first
    }
}

#[cfg(test)]
mod tests {
    use super::{ShellProcess, Unconfirmed, Verdict, watch};
    use std::sync::atomic::AtomicU8;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn exactly_one_of_a_racing_claim_and_loss_wins_and_neither_repeats() {
        let (mut claims, mut losses) = (0, 0);
        for _ in 0..1_000 {
            let shell = ShellProcess::fixture(7, Arc::new(AtomicU8::new(0)));
            let barrier = Barrier::new(2);
            let (claimed, lost) = std::thread::scope(|scope| {
                let claim = scope.spawn(|| {
                    barrier.wait();
                    shell.claim_commit().is_some()
                });
                barrier.wait();
                let lost = shell.publish(1);
                (claim.join().expect("the claim thread"), lost)
            });
            assert_ne!(claimed, lost, "exactly one side wins");
            assert!(
                shell.claim_commit().is_none(),
                "a claim is never repeated or late"
            );
            assert!(!shell.publish(1), "the order is decided once");
            claims += usize::from(claimed);
            losses += usize::from(lost);
        }
        assert_eq!(claims + losses, 1_000);
    }

    #[test]
    fn a_fence_admits_only_while_watching_and_keeps_its_refusal() {
        let shell = ShellProcess::fixture(7, Arc::new(AtomicU8::new(0)));
        assert!(shell.claim_commit().is_some());
        assert_eq!(shell.fence(), Ok(()));
        assert_eq!(shell.fence(), Ok(()), "a passed fence consumes nothing");
        shell.publish(1);
        assert!(shell.claim_commit().is_none());
        for _ in 0..2 {
            assert_eq!(shell.fence(), Err(Verdict::Exited), "a refusal stays");
        }
        let unconfirmed = ShellProcess::fixture(8, Arc::new(AtomicU8::new(2)));
        assert!(unconfirmed.claim_commit().is_some());
        assert_eq!(
            unconfirmed.fence(),
            Err(Verdict::Unconfirmed(Unconfirmed::Watch))
        );
    }

    #[test]
    fn a_loss_before_the_claim_refuses_it_for_the_rest_of_the_process() {
        let shell = ShellProcess::fixture(7, Arc::new(AtomicU8::new(0)));
        shell.publish(1);
        for _ in 0..2 {
            assert!(shell.claim_commit().is_none());
        }
        assert_eq!(shell.fence(), Err(Verdict::Exited));
    }

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
        let detached = ShellProcess::fixture(23, Arc::new(AtomicU8::new(0)));
        assert!(detached.loss().is_none(), "a fixture has no parent watch");
    }
}
