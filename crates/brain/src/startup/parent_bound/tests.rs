use std::io;
use std::sync::{Arc, Weak, mpsc};
use std::time::Duration;

use kumochi_protocol::constants::SESSION_LOCK;
use kumochi_protocol::message::CAPABILITY_LEN;
use tokio::sync::mpsc as async_mpsc;
use tokio::time::timeout;

use super::*;
use crate::engines::fixture::Fixture;
use crate::platform::SessionLockError;
use crate::runtime::{REFUSE_SPAWN, Selected};
use crate::startup::custody::SessionCustody;
use crate::startup::instance::Added;
use crate::test_support::{Scratch, held_accounts_until, unsupported_accounts};

const PATIENCE: Duration = Duration::from_secs(60);

/// An idle attempt settles without an item call, so none is made.
struct NoItem;

impl InstanceStore for NoItem {
    type Error = std::convert::Infallible;
    fn add(&self, _: &[u8; CAPABILITY_LEN]) -> Result<Added, Self::Error> {
        unreachable!("an idle attempt adds no item")
    }
    fn inspect(&self) -> Result<(), Self::Error> {
        unreachable!("an idle attempt inspects no item")
    }
    fn update(&self, _: &[u8; CAPABILITY_LEN]) -> Result<(), Self::Error> {
        unreachable!("an idle attempt updates no item")
    }
    fn read(&self, _: &mut [u8; CAPABILITY_LEN]) -> Result<(), Self::Error> {
        unreachable!("an idle attempt reads no item")
    }
    fn delete(&self) -> Result<(), Self::Error> {
        unreachable!("an idle attempt deletes no item")
    }
}

/// Signals when the credential thread drops it, after the vault and before the release, then
/// holds that thread until the test frees it, for at most `PATIENCE`.
struct Hold {
    entered: async_mpsc::UnboundedSender<()>,
    free: mpsc::Receiver<()>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.entered.send(()).ok();
        self.free.recv_timeout(PATIENCE).ok();
    }
}

/// The test's ends of a `Hold`.
struct Holding {
    entered: async_mpsc::UnboundedReceiver<()>,
    free: mpsc::Sender<()>,
}

/// A `Hold`'s parts, unarmed, so a fixture dropped before its accounts start never blocks.
fn hold() -> (
    (async_mpsc::UnboundedSender<()>, mpsc::Receiver<()>),
    Holding,
) {
    let (entered_tx, entered) = async_mpsc::unbounded_channel();
    let (free, free_rx) = mpsc::channel();
    ((entered_tx, free_rx), Holding { entered, free })
}

/// How the fixture's accounts start.
enum Start {
    /// Held accounts on the profile lock.
    Held,
    /// A startup load that finds native credentials unsupported.
    Unsupported,
}

/// A scratch store, accounts whose release waits on the hold, and a subscriber scoped to this
/// thread.
struct Scripted<'a> {
    scratch: &'a Scratch,
    start: Start,
    hold: Option<(async_mpsc::UnboundedSender<()>, mpsc::Receiver<()>)>,
}

impl<'a> Scripted<'a> {
    fn new(scratch: &'a Scratch, start: Start) -> (Self, Holding) {
        let (parts, holding) = hold();
        let pre = Self {
            scratch,
            start,
            hold: Some(parts),
        };
        (pre, holding)
    }
}

impl Prerequisites for Scripted<'_> {
    fn ready(&self) -> Result<(), NotReady> {
        Ok(())
    }

    fn init(&mut self) -> Result<ProcessInit, BrainError> {
        let logs = crate::logging::file_subscriber(&self.scratch.dir().join("logs"), "brain")
            .map_err(|_| BrainError::Process(io::ErrorKind::Other.into()))?;
        Ok(ProcessInit {
            _scoped: Some(tracing::subscriber::set_default(logs)),
        })
    }

    fn open(&mut self, acquired: &mut bool) -> Result<OpenedState, BrainError> {
        OpenedState::load_marking(
            &self.scratch.db(),
            || Some(self.scratch.dir().to_owned()),
            Some(Selected::Fixture(Box::new(
                Fixture::parse("").expect("empty fixture"),
            ))),
            acquired,
        )
    }

    fn accounts(
        &mut self,
        _: OAuthTuning,
        _: TokenTuning,
        profile: Arc<SessionLock>,
    ) -> Result<(Accounts, VaultHeld), BrainError> {
        let (entered, free) = self.hold.take().expect("one start");
        let hold = Hold { entered, free };
        let accounts = match self.start {
            Start::Held => held_accounts_until(profile, hold),
            Start::Unsupported => unsupported_accounts(hold)?,
        };
        Ok((accounts, Arc::default()))
    }
}

/// A root whose attempt holds a token custody, and a watch on that custody.
fn root() -> (Root<NoItem>, Weak<()>) {
    let token = Arc::new(());
    let custody = Arc::downgrade(&token);
    let mut root = Root::new();
    root.instance = Some(InstanceAttempt::new(
        SessionCustody::fixture(token),
        Arc::new(NoItem),
    ));
    (root, custody)
}

/// Fails the start at `stop`, if any.
fn fail_at(
    stop: Option<Checkpoint>,
) -> impl FnMut(Checkpoint, &mut Stages) -> Result<(), BrainError> {
    move |point, _| {
        if Some(point) == stop {
            Err(BrainError::Process(io::ErrorKind::Interrupted.into()))
        } else {
            Ok(())
        }
    }
}

const SETTLED: Settled = Settled {
    runtime: Shutdown {
        quit: None,
        store: Stage::Unconfirmed,
        settings: Stage::Confirmed,
        claude: Stage::Confirmed,
        account: Stage::Confirmed,
    },
    item: Stage::Confirmed,
};

/// Fails a root's start after its accounts started, at `stop` or else in `Brain::new`, and
/// checks that settling waits for the credential thread's release before custody drops.
async fn assert_settles_after_the_release(name: &str, stop: Option<Checkpoint>) {
    let scratch = Scratch::new(name);
    let lock = scratch.dir().join(SESSION_LOCK);
    let (mut root, custody) = root();
    let (mut pre, mut holding) = Scripted::new(&scratch, Start::Held);
    REFUSE_SPAWN.set(stop.is_none());
    let Err(error) = root.start(&mut pre, fail_at(stop)) else {
        panic!("the injected failure must stop the start");
    };
    match stop {
        Some(_) => assert!(
            matches!(&error, BrainError::Process(error) if error.kind() == io::ErrorKind::Interrupted),
            "{error:?}"
        ),
        None => assert!(
            matches!(&error, BrainError::StoreThread(error) if error.kind() == io::ErrorKind::Other),
            "{error:?}"
        ),
    }
    assert!(root.stages.brain.is_none(), "no Brain was built");
    let settle = root.settle();
    tokio::pin!(settle);
    tokio::select! {
        biased;
        settled = &mut settle => panic!("settled before the credential thread let go: {settled:?}"),
        entered = holding.entered.recv() => entered.expect("the credential thread ends"),
    }
    assert!(
        timeout(Duration::ZERO, &mut settle).await.is_err(),
        "settling waits for the held credential thread"
    );
    assert!(
        matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
        "a contender is refused while the release is held"
    );
    assert!(custody.upgrade().is_some(), "custody outlives the release");
    holding
        .free
        .send(())
        .expect("the credential thread waits for its release");
    let settled = timeout(PATIENCE, settle)
        .await
        .expect("settling ends once the release is free");
    assert_eq!(
        settled, SETTLED,
        "the accounts close in time and the raw store stays unconfirmed"
    );
    assert!(custody.upgrade().is_none(), "custody drops at settlement");
    drop(SessionLock::try_acquire(&lock).expect("the profile is free once settled"));
}

#[tokio::test]
async fn a_failed_accounts_checkpoint_closes_the_accounts_before_custody_drops() {
    Box::pin(assert_settles_after_the_release(
        "root-accounts-checkpoint",
        Some(Checkpoint::Accounts),
    ))
    .await;
}

#[tokio::test]
async fn a_failed_brain_constructor_returns_the_accounts_to_close_before_custody_drops() {
    Box::pin(assert_settles_after_the_release("root-brain-new", None)).await;
}

/// The worker starts, its load refuses, and the start returns only once that worker has let go,
/// so nothing it held is left for settlement to miss.
#[tokio::test]
async fn a_refused_startup_load_returns_only_after_its_worker_let_go() {
    let scratch = Scratch::new("root-unsupported-load");
    let lock = scratch.dir().join(SESSION_LOCK);
    let (mut root, custody) = root();
    let (mut pre, holding) = Scripted::new(&scratch, Start::Unsupported);
    let (returned_tx, returned) = mpsc::channel();
    let watcher = std::thread::spawn({
        let Holding { mut entered, free } = holding;
        let lock = lock.clone();
        move || {
            entered.blocking_recv().expect("the credential thread ends");
            assert!(
                matches!(SessionLock::try_acquire(&lock), Err(SessionLockError::Busy)),
                "a contender is refused while the worker is held"
            );
            assert!(custody.upgrade().is_some(), "custody outlives the worker");
            assert_eq!(
                returned.recv_timeout(Duration::from_millis(500)),
                Err(mpsc::RecvTimeoutError::Timeout),
                "the start waits for the held worker"
            );
            free.send(()).expect("the worker waits for its release");
            custody
        }
    });
    let started = root.start(&mut pre, |_, _| Ok(()));
    returned_tx.send(()).ok();
    let Err(error) = started else {
        panic!("an unsupported load must stop the start");
    };
    assert!(
        matches!(&error, BrainError::Process(error) if error.kind() == io::ErrorKind::Unsupported),
        "{error:?}"
    );
    let custody = watcher.join().expect("the watcher's checks hold");
    let settled = timeout(PATIENCE, Box::pin(root.settle()))
        .await
        .expect("settling has nothing to wait for");
    assert_eq!(
        settled, SETTLED,
        "no accounts are left to close, and the opened store stays unconfirmed"
    );
    assert!(custody.upgrade().is_none(), "custody drops at settlement");
    drop(SessionLock::try_acquire(&lock).expect("the profile is free once settled"));
}

/// An open refused before it holds a store leaves nothing to confirm.
#[tokio::test]
async fn an_open_refused_before_the_store_settles_it_confirmed() {
    let scratch = Scratch::new("root-open-busy");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    let held = SessionLock::try_acquire(&scratch.dir().join(SESSION_LOCK)).expect("the lock");
    let (mut root, _) = root();
    let (mut pre, _holding) = Scripted::new(&scratch, Start::Held);
    let Err(error) = root.start(&mut pre, fail_at(None)) else {
        panic!("a busy profile must stop the start");
    };
    assert!(matches!(error, BrainError::ProfileBusy), "{error:?}");
    let settled = Box::pin(root.settle()).await;
    assert_eq!(
        settled.runtime.store,
        Stage::Confirmed,
        "no store was opened: {settled:?}"
    );
    drop(held);
}

/// An open that fails after SQLite opened has dropped a raw store, which nothing confirms.
#[tokio::test]
async fn an_open_failed_after_the_store_settles_it_unconfirmed() {
    let scratch = Scratch::new("root-open-recovery");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    drop(crate::store::Store::open(&scratch.db()).expect("a first open"));
    rusqlite::Connection::open(scratch.db())
        .expect("raw open")
        .execute_batch("DROP TABLE threads;")
        .expect("recovery's table goes");
    let (mut root, _) = root();
    let (mut pre, _holding) = Scripted::new(&scratch, Start::Held);
    let Err(error) = root.start(&mut pre, fail_at(None)) else {
        panic!("a failed recovery must stop the start");
    };
    assert!(matches!(error, BrainError::Store(_)), "{error:?}");
    let settled = Box::pin(root.settle()).await;
    assert_eq!(
        settled.runtime.store,
        Stage::Unconfirmed,
        "the store opened before recovery failed: {settled:?}"
    );
}

#[test]
fn this_builds_prerequisites_are_never_ready() {
    assert_eq!(Pending.ready(), Err(NotReady));
}
